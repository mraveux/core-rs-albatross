use std::cmp;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::convert::TryInto;
use std::iter::{Chain, Filter, Flatten, Map};
use std::sync::Arc;
use std::vec::IntoIter;

use parking_lot::{
    MappedRwLockReadGuard, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockUpgradableReadGuard,
};

use account::inherent::AccountInherentInteraction;
use account::{Account, Inherent, InherentType};
use accounts::Accounts;
use beserial::Serialize;
use block::{
    Block, BlockBody, BlockError, BlockHeader, BlockType, ForkProof, MacroBlock, MacroBody,
    MicroBlock, ViewChange, ViewChangeProof, ViewChanges,
};
#[cfg(feature = "metrics")]
use blockchain_base::chain_metrics::BlockchainMetrics;
use blockchain_base::{AbstractBlockchain, BlockchainError, Direction};
use bls::PublicKey;
use collections::bitset::BitSet;
use database::{Environment, ReadTransaction, Transaction, WriteTransaction};
use genesis::NetworkInfo;
use hash::{Blake2bHash, Hash};
use keys::Address;
use primitives::coin::Coin;
use primitives::networks::NetworkId;
use primitives::policy;
use primitives::slot::{SlashedSlot, Slot, SlotBand, SlotIndex, Slots, ValidatorSlots};
use transaction::{Transaction as BlockchainTransaction, TransactionReceipt, TransactionsProof};
use tree_primitives::accounts_proof::AccountsProof;
use tree_primitives::accounts_tree_chunk::AccountsTreeChunk;
use utils::merkle;
use utils::observer::{Listener, ListenerHandle, Notifier};
use utils::time::OffsetTime;
use vrf::{AliasMethod, VrfSeed, VrfUseCase};

use crate::chain_info::ChainInfo;
use crate::chain_store::ChainStore;
use crate::reward::{block_reward_for_epoch, genesis_parameters};
use crate::slots::{get_slot_at, ForkProofInfos, SlashedSet};
use crate::transaction_cache::TransactionCache;

pub type PushResult = blockchain_base::PushResult;
pub type PushError = blockchain_base::PushError<BlockError>;
pub type BlockchainEvent = blockchain_base::BlockchainEvent<Block>;

pub enum OptionalCheck<T> {
    Some(T),
    None,
    Skip,
}

impl<T> OptionalCheck<T> {
    pub fn is_some(&self) -> bool {
        if let OptionalCheck::Some(_) = self {
            true
        } else {
            false
        }
    }

    pub fn is_none(&self) -> bool {
        if let OptionalCheck::None = self {
            true
        } else {
            false
        }
    }

    pub fn is_skip(&self) -> bool {
        if let OptionalCheck::Skip = self {
            true
        } else {
            false
        }
    }
}

impl<T> From<Option<T>> for OptionalCheck<T> {
    fn from(opt: Option<T>) -> Self {
        match opt {
            Some(t) => OptionalCheck::Some(t),
            None => OptionalCheck::None,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum ChainOrdering {
    Extend,
    Better,
    Inferior,
    Unknown,
}

pub enum ForkEvent {
    Detected(ForkProof),
}

pub struct Blockchain {
    pub(crate) env: Environment,
    pub network_id: NetworkId,
    pub time: Arc<OffsetTime>,
    pub notifier: RwLock<Notifier<'static, BlockchainEvent>>,
    pub fork_notifier: RwLock<Notifier<'static, ForkEvent>>,
    pub chain_store: Arc<ChainStore>,
    pub(crate) state: RwLock<BlockchainState>,
    push_lock: Mutex<()>,

    #[cfg(feature = "metrics")]
    metrics: BlockchainMetrics,

    genesis_supply: Coin,
    genesis_timestamp: u64,
}

pub struct BlockchainState {
    pub accounts: Accounts,
    pub transaction_cache: TransactionCache,

    pub(crate) main_chain: ChainInfo,
    head_hash: Blake2bHash,

    macro_info: ChainInfo,
    macro_head_hash: Blake2bHash,

    election_head: MacroBlock,
    election_head_hash: Blake2bHash,

    // TODO: Instead of Option, we could use a Cell here and use replace on it. That way we know
    //       at compile-time that there is always a valid value in there.
    current_slots: Option<Slots>,
    previous_slots: Option<Slots>,
}

impl BlockchainState {
    pub fn accounts(&self) -> &Accounts {
        &self.accounts
    }

    pub fn transaction_cache(&self) -> &TransactionCache {
        &self.transaction_cache
    }

    pub fn block_number(&self) -> u32 {
        self.main_chain.head.block_number()
    }

    pub fn current_slots(&self) -> Option<&Slots> {
        self.current_slots.as_ref()
    }

    pub fn last_slots(&self) -> Option<&Slots> {
        self.previous_slots.as_ref()
    }

    pub fn current_validators(&self) -> Option<&ValidatorSlots> {
        Some(&self.current_slots.as_ref()?.validator_slots)
    }

    pub fn last_validators(&self) -> Option<&ValidatorSlots> {
        Some(&self.previous_slots.as_ref()?.validator_slots)
    }

    /// This includes fork proof slashes and view changes.
    pub fn current_slashed_set(&self) -> BitSet {
        self.main_chain.slashed_set.current_epoch()
    }

    /// This includes fork proof slashes and view changes.
    pub fn last_slashed_set(&self) -> BitSet {
        self.main_chain.slashed_set.prev_epoch_state.clone()
    }

    pub fn main_chain(&self) -> &ChainInfo {
        &self.main_chain
    }
}

impl Blockchain {
    pub fn new(env: Environment, network_id: NetworkId) -> Result<Self, BlockchainError> {
        let chain_store = Arc::new(ChainStore::new(env.clone()));
        let time = Arc::new(OffsetTime::new());
        Ok(match chain_store.get_head(None) {
            Some(head_hash) => Blockchain::load(env, network_id, chain_store, time, head_hash)?,
            None => Blockchain::init(env, network_id, chain_store, time)?,
        })
    }

    fn load(
        env: Environment,
        network_id: NetworkId,
        chain_store: Arc<ChainStore>,
        time: Arc<OffsetTime>,
        head_hash: Blake2bHash,
    ) -> Result<Self, BlockchainError> {
        // Check that the correct genesis block is stored.
        let network_info = NetworkInfo::from_network_id(network_id);
        let genesis_info = chain_store.get_chain_info(network_info.genesis_hash(), false, None);
        if !genesis_info
            .as_ref()
            .map(|i| i.on_main_chain)
            .unwrap_or(false)
        {
            return Err(BlockchainError::InvalidGenesisBlock);
        }

        let (genesis_supply, genesis_timestamp) =
            genesis_parameters(genesis_info.unwrap().head.unwrap_macro_ref());

        // Load main chain from store.
        let main_chain = chain_store
            .get_chain_info(&head_hash, true, None)
            .ok_or(BlockchainError::FailedLoadingMainChain)?;

        // Check that chain/accounts state is consistent.
        let accounts = Accounts::new(env.clone());
        if main_chain.head.state_root() != &accounts.hash(None) {
            return Err(BlockchainError::InconsistentState);
        }

        // Load macro chain from store.
        let macro_chain_info = chain_store
            .get_chain_info_at(
                policy::last_macro_block(main_chain.head.block_number()),
                true,
                None,
            )
            .ok_or(BlockchainError::FailedLoadingMainChain)?;
        let macro_head = match macro_chain_info.head {
            Block::Macro(ref macro_head) => macro_head,
            Block::Micro(_) => return Err(BlockchainError::InconsistentState),
        };
        let macro_head_hash = macro_head.hash();

        // Load election macro chain from store.
        let election_chain_info = chain_store
            .get_chain_info_at(
                policy::last_election_block(main_chain.head.block_number()),
                true,
                None,
            )
            .ok_or(BlockchainError::FailedLoadingMainChain)?;
        let election_head = match election_chain_info.head {
            Block::Macro(macro_head) => macro_head,
            Block::Micro(_) => return Err(BlockchainError::InconsistentState),
        };

        if !election_head.is_election_block() {
            return Err(BlockchainError::InconsistentState);
        }
        let election_head_hash = election_head.hash();

        // Initialize TransactionCache.
        let mut transaction_cache = TransactionCache::new();
        let blocks = chain_store.get_blocks_backward(
            &head_hash,
            transaction_cache.missing_blocks() - 1,
            true,
            None,
        );
        for block in blocks.iter().rev() {
            transaction_cache.push_block(block);
        }
        transaction_cache.push_block(&main_chain.head);
        assert_eq!(
            transaction_cache.missing_blocks(),
            policy::TRANSACTION_VALIDITY_WINDOW_ALBATROSS
                .saturating_sub(main_chain.head.block_number() + 1)
        );

        // Current slots and validators
        let current_slots = Self::slots_from_block(&election_head);

        // Get last slots and validators
        let prev_block =
            chain_store.get_block(&election_head.header.parent_election_hash, true, None);
        let last_slots = match prev_block {
            Some(Block::Macro(prev_election_block)) => {
                if prev_election_block.is_election_block() {
                    Self::slots_from_block(&prev_election_block)
                } else {
                    return Err(BlockchainError::InconsistentState);
                }
            }
            None => Slots::default(),
            _ => return Err(BlockchainError::InconsistentState),
        };

        Ok(Blockchain {
            env,
            network_id,
            time,
            notifier: RwLock::new(Notifier::new()),
            fork_notifier: RwLock::new(Notifier::new()),
            chain_store,
            state: RwLock::new(BlockchainState {
                accounts,
                transaction_cache,
                main_chain,
                head_hash,
                macro_info: macro_chain_info,
                macro_head_hash,
                election_head,
                election_head_hash,
                current_slots: Some(current_slots),
                previous_slots: Some(last_slots),
            }),
            push_lock: Mutex::new(()),

            #[cfg(feature = "metrics")]
            metrics: BlockchainMetrics::default(),
            genesis_supply,
            genesis_timestamp,
        })
    }

    fn init(
        env: Environment,
        network_id: NetworkId,
        chain_store: Arc<ChainStore>,
        time: Arc<OffsetTime>,
    ) -> Result<Self, BlockchainError> {
        // Initialize chain & accounts with genesis block.
        let network_info = NetworkInfo::from_network_id(network_id);
        let genesis_block = network_info.genesis_block::<Block>();
        let genesis_macro_block = (match genesis_block {
            Block::Macro(ref macro_block) => Some(macro_block),
            _ => None,
        })
        .unwrap();
        let (genesis_supply, genesis_timestamp) = genesis_parameters(genesis_macro_block);
        let main_chain = ChainInfo::initial(genesis_block.clone());
        let head_hash = network_info.genesis_hash().clone();

        // Initialize accounts.
        let accounts = Accounts::new(env.clone());
        let mut txn = WriteTransaction::new(&env);
        accounts.init(&mut txn, network_info.genesis_accounts());

        // Commit genesis block to accounts.
        // XXX Don't distribute any reward for the genesis block, so there is nothing to commit.

        // Store genesis block.
        chain_store.put_chain_info(&mut txn, &head_hash, &main_chain, true);
        chain_store.set_head(&mut txn, &head_hash);
        txn.commit();

        // Initialize empty TransactionCache.
        let transaction_cache = TransactionCache::new();

        // current slots and validators
        let current_slots = Self::slots_from_block(&genesis_macro_block);
        let last_slots = Slots::default();

        Ok(Blockchain {
            env,
            network_id,
            time,
            notifier: RwLock::new(Notifier::new()),
            fork_notifier: RwLock::new(Notifier::new()),
            chain_store,
            state: RwLock::new(BlockchainState {
                accounts,
                transaction_cache,
                macro_info: main_chain.clone(),
                main_chain,
                head_hash: head_hash.clone(),
                macro_head_hash: head_hash.clone(),
                election_head: genesis_macro_block.clone(),
                election_head_hash: head_hash,
                current_slots: Some(current_slots.clone()),
                previous_slots: Some(last_slots),
            }),
            push_lock: Mutex::new(()),

            #[cfg(feature = "metrics")]
            metrics: BlockchainMetrics::default(),
            genesis_supply,
            genesis_timestamp,
        })
    }

    fn slots_from_block(block: &MacroBlock) -> Slots {
        block.clone().try_into().unwrap()
    }

    pub fn get_slots_for_epoch(&self, epoch: u32) -> Option<Slots> {
        let state = self.state.read();
        let current_epoch = policy::epoch_at(state.main_chain.head.block_number());

        let slots = if epoch == current_epoch {
            state.current_slots.as_ref()?.clone()
        } else if epoch == current_epoch - 1 {
            state.previous_slots.as_ref()?.clone()
        } else {
            let macro_block = self
                .get_block_at(policy::election_block_of(epoch), true)?
                .unwrap_macro();
            macro_block.try_into().unwrap()
        };

        Some(slots)
    }

    pub fn get_validators_for_epoch(&self, epoch: u32) -> Option<ValidatorSlots> {
        let state = self.state.read();
        let current_epoch = policy::epoch_at(state.main_chain.head.block_number());

        let validators = if epoch == current_epoch {
            state.current_validators()?.clone()
        } else if epoch == current_epoch - 1 {
            state.last_validators()?.clone()
        } else {
            self.get_block_at(policy::election_block_of(epoch), true)?
                .unwrap_macro()
                .body
                .expect("Missing body!")
                .validators?
                .into()
        };

        Some(validators)
    }

    pub fn verify_block_header(
        &self,
        header: &BlockHeader,
        view_change_proof: OptionalCheck<&ViewChangeProof>,
        intended_slot_owner: &MappedRwLockReadGuard<PublicKey>,
        txn_opt: Option<&Transaction>,
    ) -> Result<(), PushError> {
        // Check if the block's immediate predecessor is part of the chain.
        let prev_info_opt = self
            .chain_store
            .get_chain_info(&header.parent_hash(), false, txn_opt);
        if prev_info_opt.is_none() {
            warn!("Rejecting block - unknown predecessor");
            return Err(PushError::Orphan);
        }

        // Check that the block is a valid successor of its predecessor.
        let prev_info = prev_info_opt.unwrap();

        if self.get_next_block_type(Some(prev_info.head.block_number())) != header.ty() {
            warn!("Rejecting block - wrong block type ({:?})", header.ty());
            return Err(PushError::InvalidSuccessor);
        }

        // Check the block number
        if prev_info.head.block_number() + 1 != header.block_number() {
            warn!(
                "Rejecting block - wrong block number ({:?})",
                header.block_number()
            );
            return Err(PushError::InvalidSuccessor);
        }

        // Check that the current block timestamp is equal or greater than the timestamp of the
        // previous block.
        if prev_info.head.timestamp() >= header.timestamp() {
            warn!("Rejecting block - block timestamp precedes parent timestamp");
            return Err(PushError::InvalidSuccessor);
        }

        // Check that the current block timestamp less the node's current time is less than or equal
        // to the allowed maximum drift. Basically, we check that the block isn't from the future.
        // Both times are given in Unix time standard.
        if header.timestamp().saturating_sub(self.time.now()) > policy::TIMESTAMP_MAX_DRIFT {
            warn!("Rejecting block - block timestamp exceeds allowed maximum drift");
            return Err(PushError::InvalidBlock(BlockError::FromTheFuture));
        }

        // Check if a view change occurred - if so, validate the proof
        let view_number = if policy::is_macro_block_at(header.block_number() - 1) {
            0 // Reset view number in new batch
        } else {
            prev_info.head.view_number()
        };

        let new_view_number = header.view_number();
        if new_view_number < view_number {
            warn!(
                "Rejecting block - lower view number {:?} < {:?}",
                header.view_number(),
                view_number
            );
            return Err(PushError::InvalidBlock(BlockError::InvalidViewNumber));
        } else if new_view_number > view_number {
            match view_change_proof {
                OptionalCheck::None => {
                    warn!("Rejecting block - missing view change proof");
                    return Err(PushError::InvalidBlock(BlockError::NoViewChangeProof));
                }
                OptionalCheck::Some(ref view_change_proof) => {
                    let view_change = ViewChange {
                        block_number: header.block_number(),
                        new_view_number: header.view_number(),
                        prev_seed: prev_info.head.seed().clone(),
                    };
                    if let Err(e) = view_change_proof.verify(
                        &view_change,
                        &self.current_validators(),
                        policy::TWO_THIRD_SLOTS,
                    ) {
                        warn!("Rejecting block - bad view change proof: {:?}", e);
                        return Err(PushError::InvalidBlock(BlockError::InvalidJustification));
                    }
                }
                OptionalCheck::Skip => {}
            }
        } else if new_view_number == view_number && view_change_proof.is_some() {
            warn!("Rejecting block - must not contain view change proof");
            return Err(PushError::InvalidBlock(BlockError::InvalidJustification));
        }

        // Check if the block was produced (and signed) by the intended producer
        if let Err(e) = header
            .seed()
            .verify(prev_info.head.seed(), intended_slot_owner)
        {
            warn!("Rejecting block - invalid seed ({:?})", e);
            return Err(PushError::InvalidBlock(BlockError::InvalidJustification));
        }

        if let BlockHeader::Macro(ref header) = header {
            // TODO: Move this check to someplace else!
            // // In case of an election block make sure it contains validators
            // if policy::is_election_block_at(header.block_number)
            //     && body.unwrap_macro().validators.is_none()
            // {
            //     return Err(PushError::InvalidSuccessor);
            // }

            // Check if the parent election hash matches the current election head hash
            if header.parent_election_hash != self.state().election_head_hash {
                warn!("Rejecting block - wrong parent election hash");
                return Err(PushError::InvalidSuccessor);
            }

            // TODO: Move this check to someplace else!
            // let history_root = self
            //     .get_history_root(policy::batch_at(header.block_number), txn_opt)
            //     .ok_or(PushError::BlockchainError(
            //         BlockchainError::FailedLoadingMainChain,
            //     ))?;
            // if body.unwrap_macro().history_root != history_root {
            //     warn!("Rejecting block - wrong history root");
            //     return Err(PushError::InvalidBlock(BlockError::InvalidHistoryRoot));
            // }
        }

        Ok(())
    }

    pub fn push(&self, block: Block) -> Result<PushResult, PushError> {
        self.push_block(block)
    }

    /// Calculate chain ordering.
    fn order_chains(
        &self,
        block: &Block,
        prev_info: &ChainInfo,
        txn_option: Option<&Transaction>,
    ) -> ChainOrdering {
        let mut chain_order = ChainOrdering::Unknown;
        if block.parent_hash() == &self.head_hash() {
            chain_order = ChainOrdering::Extend;
        } else {
            // To compare two blocks, we need to compare the view number at the intersection.
            //   [2] - [2] - [3] - [4]
            //      \- [3] - [3] - [3]
            // The example above illustrates that you actually want to choose the lower chain,
            // since its view change happened way earlier.
            // Let's thus find the first block on the branch (not on the main chain).
            // If there is a malicious fork, it might happen that the two view numbers before
            // the branch are the same. Then, we need to follow and compare.
            let mut view_numbers = vec![block.view_number()];
            let mut current: (Blake2bHash, ChainInfo) =
                (block.hash(), ChainInfo::dummy(block.clone()));
            let mut prev: (Blake2bHash, ChainInfo) = (prev_info.head.hash(), prev_info.clone());
            while !prev.1.on_main_chain {
                // Macro blocks are final
                assert_eq!(
                    prev.1.head.ty(),
                    BlockType::Micro,
                    "Trying to rebranch across macro block"
                );

                view_numbers.push(prev.1.head.view_number());

                let prev_hash = prev.1.head.parent_hash().clone();
                let prev_info = self
                    .chain_store
                    .get_chain_info(&prev_hash, false, txn_option)
                    .expect("Corrupted store: Failed to find fork predecessor while rebranching");

                current = prev;
                prev = (prev_hash, prev_info);
            }

            // Now follow the view numbers back until you find one that differs.
            // Example:
            // [0] - [0] - [1]  *correct chain*
            //    \- [0] - [0]
            // Otherwise take the longest:
            // [0] - [0] - [1] - [0]  *correct chain*
            //    \- [0] - [1]
            let current_height = current.1.head.block_number();
            let min_height = cmp::min(self.head_height(), block.block_number());

            // Iterate over common block heights starting from right after the intersection.
            for h in current_height..=min_height {
                // Take corresponding view number from branch.
                let branch_view_number = view_numbers.pop().unwrap();
                // And calculate equivalent on main chain.
                let current_on_main_chain = self
                    .chain_store
                    .get_block_at(h, false, txn_option)
                    .expect("Corrupted store: Failed to find main chain equivalent of fork");

                // Choose better one as early as possible.
                match current_on_main_chain.view_number().cmp(&branch_view_number) {
                    Ordering::Less => {
                        chain_order = ChainOrdering::Better;
                        break;
                    }
                    Ordering::Greater => {
                        chain_order = ChainOrdering::Inferior;
                        break;
                    }
                    Ordering::Equal => {} // Continue...
                }
            }

            // If they were all equal, choose the longer one.
            if chain_order == ChainOrdering::Unknown && self.head_height() < block.block_number() {
                chain_order = ChainOrdering::Better;
            }

            info!(
                "New block is on {:?} chain with fork at #{} (current #{}.{}, new block #{}.{})",
                chain_order,
                current_height - 1,
                self.head_height(),
                self.view_number(),
                block.block_number(),
                block.view_number()
            );
        }

        chain_order
    }

    /// Same as push, but with more options.
    pub fn push_block(&self, block: Block) -> Result<PushResult, PushError> {
        // Only one push operation at a time.
        let _push_lock = self.push_lock.lock();

        // XXX We might want to pass this as argument to this method
        let read_txn = ReadTransaction::new(&self.env);

        // Check if we already know this block.
        let hash: Blake2bHash = block.hash();
        if self
            .chain_store
            .get_chain_info(&hash, false, Some(&read_txn))
            .is_some()
        {
            return Ok(PushResult::Known);
        }

        // Check (sort of) intrinsic block invariants.
        if let Err(e) = block.verify(self.network_id) {
            warn!("Rejecting block - verification failed ({:?})", e);
            return Err(PushError::InvalidBlock(e));
        }

        let prev_info = if let Some(prev_info) =
            self.chain_store
                .get_chain_info(&block.parent_hash(), false, Some(&read_txn))
        {
            prev_info
        } else {
            warn!(
                "Rejecting block - unknown predecessor (#{}, current #{})",
                block.header().block_number(),
                self.state.read().main_chain.head.block_number()
            );
            #[cfg(feature = "metrics")]
            self.metrics.note_orphan_block();
            return Err(PushError::Orphan);
        };

        // We have to be careful if the previous block is on a branch!
        // In this case `get_slot_at` would result in wrong slots.
        // Luckily, Albatross has the nice property that branches can only happen through
        // view changes or forks.
        // If it is a view change, we can always decide at the intersection which one is better.
        // We never even try to push on inferior branches, so we need to check this early.
        // Forks either maintain the same view numbers (and thus the same slots)
        // or there is a difference in view numbers on the way and we can discard the inferior branch.
        // This could potentially change later on, but as forks have the same slots,
        // we can always identify the inferior branch.
        // This is also the reason why fork proofs do not change the slashed set for validator selection.
        // Here, we identify inferior branches early on and discard them.
        let chain_order = self.order_chains(&block, &prev_info, Some(&read_txn));
        if chain_order == ChainOrdering::Inferior {
            // If it is an inferior chain, we ignore it as it cannot become better at any point in time.
            info!(
                "Ignoring block - inferior chain (#{}, {})",
                block.block_number(),
                hash
            );
            return Ok(PushResult::Ignored);
        }

        let view_change_proof = match block {
            Block::Macro(_) => OptionalCheck::Skip,
            Block::Micro(ref micro_block) => micro_block
                .justification
                .as_ref()
                .expect("Missing body!")
                .view_change_proof
                .as_ref()
                .into(),
        };

        let (slot, _) = self
            .get_slot_at(block.block_number(), block.view_number(), Some(&read_txn))
            .unwrap();

        {
            let intended_slot_owner = slot.public_key().uncompress_unchecked();
            // This will also check that the type at this block number is correct and whether or not an election takes place
            if let Err(e) = self.verify_block_header(
                &block.header(),
                view_change_proof,
                &intended_slot_owner,
                Some(&read_txn),
            ) {
                warn!("Rejecting block - Bad header / justification");
                return Err(e);
            }
        }

        if let Block::Micro(ref micro_block) = block {
            let justification = match micro_block
                .justification
                .as_ref()
                .expect("Missing justification!")
                .signature
                .uncompress()
            {
                Ok(justification) => justification,
                Err(_) => {
                    warn!("Rejecting block - bad justification");
                    return Err(PushError::InvalidBlock(BlockError::InvalidJustification));
                }
            };

            let intended_slot_owner = slot.public_key().uncompress_unchecked();
            if !intended_slot_owner.verify(&micro_block.header, &justification) {
                warn!("Rejecting block - invalid justification for intended slot owner");
                debug!("Block hash: {}", micro_block.header.hash::<Blake2bHash>());
                debug!("Intended slot owner: {:?}", intended_slot_owner.compress());
                return Err(PushError::InvalidBlock(BlockError::InvalidJustification));
            }

            // Check if there are two blocks in the same slot and with the same height. Since we already
            // verified the validator for the current slot, this is enough to check for fork proofs.
            // Count the micro blocks after the last macro block.
            let mut micro_blocks: Vec<Block> =
                self.chain_store
                    .get_blocks_at(block.block_number(), false, Some(&read_txn));

            // Get the micro header from the block
            let micro_header1 = &micro_block.header;

            // Get the justification for the block. We assume that the
            // validator's signature is valid.
            let justification1 = &micro_block
                .justification
                .as_ref()
                .expect("Missing justification!")
                .signature;

            // Get the view number from the block
            let view_number = block.view_number();

            for micro_block in micro_blocks.drain(..).map(|block| block.unwrap_micro()) {
                // If there's another micro block set to this view number, we
                // notify the fork event.
                if view_number == micro_block.header.view_number {
                    let micro_header2 = micro_block.header;
                    let justification2 = micro_block
                        .justification
                        .as_ref()
                        .expect("Missing justification!")
                        .signature;

                    let proof = ForkProof {
                        header1: micro_header1.clone(),
                        header2: micro_header2,
                        justification1: justification1.clone(),
                        justification2,
                    };

                    self.fork_notifier.read().notify(ForkEvent::Detected(proof));
                }
            }

            // Validate slash inherents
            for fork_proof in &micro_block.body.as_ref().unwrap().fork_proofs {
                // NOTE: if this returns None, that means that at least the previous block doesn't exist, so that fork proof is invalid anyway.
                let (slot, _) = self
                    .get_slot_at(
                        fork_proof.header1.block_number,
                        fork_proof.header1.view_number,
                        Some(&read_txn),
                    )
                    .ok_or(PushError::InvalidSuccessor)?;

                if fork_proof
                    .verify(&slot.public_key().uncompress_unchecked())
                    .is_err()
                {
                    warn!("Rejecting block - Bad fork proof: invalid owner signature");
                    return Err(PushError::InvalidSuccessor);
                }
            }
        }

        if let Block::Macro(ref macro_block) = block {
            // Check Macro Justification
            match macro_block.justification {
                None => {
                    warn!("Rejecting block - macro block without justification");
                    return Err(PushError::InvalidBlock(BlockError::NoJustification));
                }
                Some(ref justification) => {
                    if let Err(e) = justification.verify(
                        macro_block.hash(),
                        &self.current_validators(),
                        policy::TWO_THIRD_SLOTS,
                    ) {
                        warn!(
                            "Rejecting block - macro block with bad justification: {}",
                            e
                        );
                        return Err(PushError::InvalidBlock(BlockError::InvalidJustification));
                    }
                }
            }

            // The correct construction of the extrinsics is only checked after the block's inherents are applied.

            // The macro body cannot be None.
            if let Some(ref body) = macro_block.body {
                let body_hash: Blake2bHash = body.hash();
                if body_hash != macro_block.header.body_root {
                    warn!("Rejecting block - Header body hash doesn't match real body hash");
                    return Err(PushError::InvalidBlock(BlockError::BodyHashMismatch));
                }
            }
        }

        let fork_proof_infos = ForkProofInfos::fetch(&block, &self.chain_store, Some(&read_txn))
            .map_err(|err| {
                warn!("Rejecting block - slash commit failed: {:?}", err);
                PushError::InvalidSuccessor
            })?;
        let chain_info = match ChainInfo::new(block, &prev_info, &fork_proof_infos) {
            Ok(info) => info,
            Err(err) => {
                warn!("Rejecting block - slash commit failed: {:?}", err);
                return Err(PushError::InvalidSuccessor);
            }
        };

        // Drop read transaction before calling other functions.
        drop(read_txn);

        match chain_order {
            ChainOrdering::Extend => {
                return self.extend(chain_info.head.hash(), chain_info, prev_info);
            }
            ChainOrdering::Better => {
                return self.rebranch(chain_info.head.hash(), chain_info);
            }
            ChainOrdering::Inferior => unreachable!(),
            ChainOrdering::Unknown => {} // Continue.
        }

        // Otherwise, we are creating/extending a fork. Store ChainInfo.
        debug!(
            "Creating/extending fork with block {}, block number #{}, view number {}",
            chain_info.head.hash(),
            chain_info.head.block_number(),
            chain_info.head.view_number()
        );
        let mut txn = WriteTransaction::new(&self.env);
        self.chain_store
            .put_chain_info(&mut txn, &chain_info.head.hash(), &chain_info, true);
        txn.commit();

        Ok(PushResult::Forked)
    }

    fn extend(
        &self,
        block_hash: Blake2bHash,
        mut chain_info: ChainInfo,
        mut prev_info: ChainInfo,
    ) -> Result<PushResult, PushError> {
        let mut txn = WriteTransaction::new(&self.env);
        let state = self.state.read();

        // Check transactions against TransactionCache to prevent replay.
        // XXX This is technically unnecessary for macro blocks, but it doesn't hurt either.
        if state.transaction_cache.contains_any(&chain_info.head) {
            warn!("Rejecting block - transaction already included");
            txn.abort();
            return Err(PushError::DuplicateTransaction);
        }

        // Commit block to AccountsTree.
        if let Err(e) = self.commit_accounts(
            &state,
            prev_info.head.next_view_number(),
            &mut txn,
            &chain_info,
        ) {
            warn!("Rejecting block - commit failed: {:?}", e);
            txn.abort();
            #[cfg(feature = "metrics")]
            self.metrics.note_invalid_block();
            return Err(e);
        }

        drop(state);

        // Only now can we check the macro body.
        let mut is_election_block = false;
        if let Block::Macro(ref mut macro_block) = &mut chain_info.head {
            is_election_block = macro_block.is_election_block();

            if is_election_block {
                let slots = self.next_slots(&macro_block.header.seed, Some(&txn));
                if let Some(ref block_slots) =
                    macro_block.body.as_ref().expect("Missing body!").validators
                {
                    if &slots.validator_slots != block_slots {
                        warn!("Rejecting block - Validators don't match real validators");
                        return Err(PushError::InvalidBlock(BlockError::InvalidValidators));
                    }
                } else {
                    warn!("Rejecting block - Validators missing");
                    return Err(PushError::InvalidBlock(BlockError::InvalidValidators));
                }
            }

            // The final list of slashes from the previous epoch.
            let slashed_set = chain_info.slashed_set.prev_epoch_state.clone();
            // Macro blocks which do not have an election also need to keep track of the slashed set of the current
            // epoch. Macro blocks with an election always have a current_slashed_set of None, as slashes are reset
            // on election.
            let current_slashed_set = chain_info.slashed_set.current_epoch();
            let mut computed_extrinsics =
                MacroBody::from_slashed_set(slashed_set, current_slashed_set);
            let computed_extrinsics_hash: Blake2bHash = computed_extrinsics.hash();
            if computed_extrinsics_hash != macro_block.header.body_root {
                warn!("Rejecting block - Extrinsics hash doesn't match real extrinsics hash");
                return Err(PushError::InvalidBlock(BlockError::BodyHashMismatch));
            }
        }

        chain_info.on_main_chain = true;
        prev_info.main_chain_successor = Some(chain_info.head.hash());

        self.chain_store
            .put_chain_info(&mut txn, &block_hash, &chain_info, true);
        self.chain_store.put_chain_info(
            &mut txn,
            &chain_info.head.parent_hash(),
            &prev_info,
            false,
        );
        self.chain_store.set_head(&mut txn, &block_hash);

        // Acquire write lock & commit changes.
        let mut state = self.state.write();
        state.transaction_cache.push_block(&chain_info.head);

        if let Block::Macro(ref macro_block) = chain_info.head {
            state.macro_info = chain_info.clone();
            state.macro_head_hash = block_hash.clone();

            if is_election_block {
                state.election_head = macro_block.clone();
                state.election_head_hash = block_hash.clone();

                let slots = state.current_slots.take().unwrap();
                state.previous_slots.replace(slots);

                let slot = Self::slots_from_block(&macro_block);
                state.current_slots.replace(slot);
            }
        }

        state.main_chain = chain_info;
        state.head_hash = block_hash.clone();
        txn.commit();

        // Give up lock before notifying.
        drop(state);

        if is_election_block {
            self.notifier
                .read()
                .notify(BlockchainEvent::EpochFinalized(block_hash));
        } else {
            self.notifier
                .read()
                .notify(BlockchainEvent::Finalized(block_hash));
        }

        Ok(PushResult::Extended)
    }

    fn rebranch(
        &self,
        block_hash: Blake2bHash,
        chain_info: ChainInfo,
    ) -> Result<PushResult, PushError> {
        debug!(
            "Rebranching to fork {}, height #{}, view number {}",
            block_hash,
            chain_info.head.block_number(),
            chain_info.head.view_number()
        );

        // Find the common ancestor between our current main chain and the fork chain.
        // Walk up the fork chain until we find a block that is part of the main chain.
        // Store the chain along the way.
        let read_txn = ReadTransaction::new(&self.env);

        let mut fork_chain: Vec<(Blake2bHash, ChainInfo)> = vec![];
        let mut current: (Blake2bHash, ChainInfo) = (block_hash, chain_info);
        while !current.1.on_main_chain {
            // A fork can't contain a macro block. We already received that macro block, thus it must be on our
            // main chain.
            assert_eq!(
                current.1.head.ty(),
                BlockType::Micro,
                "Fork contains macro block"
            );

            let prev_hash = current.1.head.parent_hash().clone();
            let prev_info = self
                .chain_store
                .get_chain_info(&prev_hash, true, Some(&read_txn))
                .expect("Corrupted store: Failed to find fork predecessor while rebranching");

            fork_chain.push(current);
            current = (prev_hash, prev_info);
        }

        debug!(
            "Found common ancestor {} at height #{}, {} blocks up",
            current.0,
            current.1.head.block_number(),
            fork_chain.len()
        );

        // Revert AccountsTree & TransactionCache to the common ancestor state.
        let mut revert_chain: Vec<(Blake2bHash, ChainInfo)> = vec![];
        let mut ancestor = current;

        let mut write_txn = WriteTransaction::new(&self.env);
        let mut cache_txn;

        let state = self.state.upgradable_read();
        cache_txn = state.transaction_cache.clone();
        // XXX Get rid of the .clone() here.
        current = (state.head_hash.clone(), state.main_chain.clone());

        // Check if ancestor is in current epoch
        if ancestor.1.head.block_number() < state.macro_info.head.block_number() {
            info!("Ancestor is in finalized epoch");
            return Err(PushError::InvalidFork);
        }

        while current.0 != ancestor.0 {
            match current.1.head {
                Block::Macro(_) => {
                    // Macro blocks are final, we can't revert across them. This should be checked before we start reverting
                    panic!("Trying to rebranch across macro block");
                }
                Block::Micro(ref micro_block) => {
                    let prev_hash = micro_block.header.parent_hash.clone();
                    let prev_info = self.chain_store
                        .get_chain_info(&prev_hash, true, Some(&read_txn))
                        .expect("Corrupted store: Failed to find main chain predecessor while rebranching");

                    self.revert_accounts(
                        &state.accounts,
                        &mut write_txn,
                        &micro_block,
                        prev_info.head.view_number(),
                    )?;

                    cache_txn.revert_block(&current.1.head);

                    assert_eq!(
                        prev_info.head.state_root(),
                        &state.accounts.hash(Some(&write_txn)),
                        "Failed to revert main chain while rebranching - inconsistent state"
                    );

                    revert_chain.push(current);
                    current = (prev_hash, prev_info);
                }
            }
        }

        // Fetch missing blocks for TransactionCache.
        assert!(cache_txn.is_empty() || cache_txn.head_hash() == ancestor.0);
        let start_hash = if cache_txn.is_empty() {
            ancestor.1.main_chain_successor.unwrap()
        } else {
            cache_txn.tail_hash()
        };
        let blocks = self.chain_store.get_blocks_backward(
            &start_hash,
            cache_txn.missing_blocks(),
            true,
            Some(&read_txn),
        );
        for block in blocks.iter() {
            cache_txn.prepend_block(block)
        }
        assert_eq!(
            cache_txn.missing_blocks(),
            policy::TRANSACTION_VALIDITY_WINDOW_ALBATROSS
                .saturating_sub(ancestor.1.head.block_number() + 1)
        );

        // Check each fork block against TransactionCache & commit to AccountsTree and SlashRegistry.
        let mut prev_view_number = ancestor.1.head.next_view_number();
        let mut fork_iter = fork_chain.iter().rev();
        while let Some(fork_block) = fork_iter.next() {
            match fork_block.1.head {
                Block::Macro(_) => unreachable!(),
                Block::Micro(ref micro_block) => {
                    let result = if !cache_txn.contains_any(&fork_block.1.head) {
                        self.commit_accounts(
                            &state,
                            prev_view_number,
                            &mut write_txn,
                            &fork_block.1,
                        )
                    } else {
                        Err(PushError::DuplicateTransaction)
                    };

                    if let Err(e) = result {
                        warn!("Failed to apply fork block while rebranching - {:?}", e);
                        write_txn.abort();

                        // Delete invalid fork blocks from store.
                        let mut write_txn = WriteTransaction::new(&self.env);
                        for block in vec![fork_block].into_iter().chain(fork_iter) {
                            self.chain_store.remove_chain_info(
                                &mut write_txn,
                                &block.0,
                                micro_block.header.block_number,
                            )
                        }
                        write_txn.commit();

                        return Err(PushError::InvalidFork);
                    }

                    cache_txn.push_block(&fork_block.1.head);
                    prev_view_number = fork_block.1.head.next_view_number();
                }
            }
        }

        // Fork looks good.
        // Drop read transaction.
        read_txn.close();

        // Acquire write lock.
        let mut state = RwLockUpgradableReadGuard::upgrade(state);

        // Unset onMainChain flag / mainChainSuccessor on the current main chain up to (excluding) the common ancestor.
        for reverted_block in revert_chain.iter_mut() {
            reverted_block.1.on_main_chain = false;
            reverted_block.1.main_chain_successor = None;
            self.chain_store.put_chain_info(
                &mut write_txn,
                &reverted_block.0,
                &reverted_block.1,
                false,
            );
        }

        // Update the mainChainSuccessor of the common ancestor block.
        ancestor.1.main_chain_successor = Some(fork_chain.last().unwrap().0.clone());
        self.chain_store
            .put_chain_info(&mut write_txn, &ancestor.0, &ancestor.1, false);

        // Set onMainChain flag / mainChainSuccessor on the fork.
        for i in (0..fork_chain.len()).rev() {
            let main_chain_successor = if i > 0 {
                Some(fork_chain[i - 1].0.clone())
            } else {
                None
            };

            let fork_block = &mut fork_chain[i];
            fork_block.1.on_main_chain = true;
            fork_block.1.main_chain_successor = main_chain_successor;

            // Include the body of the new block (at position 0).
            self.chain_store
                .put_chain_info(&mut write_txn, &fork_block.0, &fork_block.1, i == 0);
        }

        // Commit transaction & update head.
        self.chain_store.set_head(&mut write_txn, &fork_chain[0].0);
        state.transaction_cache = cache_txn;
        state.main_chain = fork_chain[0].1.clone();
        state.head_hash = fork_chain[0].0.clone();
        write_txn.commit();

        // Give up lock before notifying.
        drop(state);

        let mut reverted_blocks = Vec::with_capacity(revert_chain.len());
        for (hash, chain_info) in revert_chain.into_iter().rev() {
            reverted_blocks.push((hash, chain_info.head));
        }
        let mut adopted_blocks = Vec::with_capacity(fork_chain.len());
        for (hash, chain_info) in fork_chain.into_iter().rev() {
            adopted_blocks.push((hash, chain_info.head));
        }
        let event = BlockchainEvent::Rebranched(reverted_blocks, adopted_blocks);
        self.notifier.read().notify(event);

        Ok(PushResult::Rebranched)
    }

    fn commit_accounts(
        &self,
        state: &BlockchainState,
        first_view_number: u32,
        txn: &mut WriteTransaction,
        chain_info: &ChainInfo,
    ) -> Result<(), PushError> {
        let block = &chain_info.head;
        let accounts = &state.accounts;

        match block {
            Block::Macro(ref macro_block) => {
                let mut inherents: Vec<Inherent> = vec![];
                if macro_block.is_election_block() {
                    // On election the previous epoch needs to be finalized.
                    // We can rely on `state` here, since we cannot revert macro blocks.
                    inherents.append(&mut self.finalize_previous_epoch(state, chain_info));
                }

                // Add slashes for view changes.
                let view_changes = ViewChanges::new(
                    macro_block.header.block_number,
                    first_view_number,
                    macro_block.header.view_number,
                );
                inherents.append(&mut self.create_slash_inherents(&[], &view_changes, Some(txn)));

                // Commit block to AccountsTree.
                let receipts =
                    accounts.commit(txn, &[], &inherents, macro_block.header.block_number);

                // macro blocks are final and receipts for the previous batch are no longer necessary
                // as rebranching across this block is not possible
                self.chain_store.clear_receipts(txn);
                if let Err(e) = receipts {
                    return Err(PushError::AccountsError(e));
                }
            }
            Block::Micro(ref micro_block) => {
                let extrinsics = micro_block.body.as_ref().unwrap();
                let view_changes = ViewChanges::new(
                    micro_block.header.block_number,
                    first_view_number,
                    micro_block.header.view_number,
                );
                let inherents =
                    self.create_slash_inherents(&extrinsics.fork_proofs, &view_changes, Some(txn));

                // Commit block to AccountsTree.
                let receipts = accounts.commit(
                    txn,
                    &extrinsics.transactions,
                    &inherents,
                    micro_block.header.block_number,
                );
                if let Err(e) = receipts {
                    return Err(PushError::AccountsError(e));
                }

                // Store receipts.
                let receipts = receipts.unwrap();
                self.chain_store
                    .put_receipts(txn, micro_block.header.block_number, &receipts);
            }
        }

        // Verify accounts hash.
        let accounts_hash = accounts.hash(Some(&txn));
        trace!("Block state root: {}", block.state_root());
        trace!("Accounts hash:    {}", accounts_hash);
        if block.state_root() != &accounts_hash {
            return Err(PushError::InvalidBlock(BlockError::AccountsHashMismatch));
        }

        Ok(())
    }

    fn revert_accounts(
        &self,
        accounts: &Accounts,
        txn: &mut WriteTransaction,
        micro_block: &MicroBlock,
        prev_view_number: u32,
    ) -> Result<(), PushError> {
        assert_eq!(
            micro_block.header.state_root,
            accounts.hash(Some(&txn)),
            "Failed to revert - inconsistent state"
        );

        let extrinsics = micro_block.body.as_ref().unwrap();
        let view_changes = ViewChanges::new(
            micro_block.header.block_number,
            prev_view_number,
            micro_block.header.view_number,
        );
        let inherents =
            self.create_slash_inherents(&extrinsics.fork_proofs, &view_changes, Some(txn));
        let receipts = self
            .chain_store
            .get_receipts(micro_block.header.block_number, Some(txn))
            .expect("Failed to revert - missing receipts");

        if let Err(e) = accounts.revert(
            txn,
            &extrinsics.transactions,
            &inherents,
            micro_block.header.block_number,
            &receipts,
        ) {
            panic!("Failed to revert - {}", e);
        }

        Ok(())
    }

    /// Pushes a macro block without requiring the micro blocks of the previous batch.
    pub fn push_isolated_macro_block(
        &self,
        block: Block,
        transactions: &[BlockchainTransaction],
    ) -> Result<PushResult, PushError> {
        // TODO: Deduplicate code as much as possible...
        // Only one push operation at a time.
        let push_lock = self.push_lock.lock();

        // XXX We might want to pass this as argument to this method
        let read_txn = ReadTransaction::new(&self.env);

        let macro_block = if let Block::Macro(ref block) = block {
            if policy::is_election_block_at(block.header.block_number) != block.is_election_block()
            {
                return Err(PushError::InvalidSuccessor);
            } else {
                block
            }
        } else {
            return Err(PushError::InvalidSuccessor);
        };

        // Check if we already know this block.
        let hash: Blake2bHash = block.hash();
        if self
            .chain_store
            .get_chain_info(&hash, false, Some(&read_txn))
            .is_some()
        {
            return Ok(PushResult::Known);
        }

        if macro_block.is_election_block()
            && self.election_head_hash() != macro_block.header.parent_election_hash
        {
            warn!("Rejecting block - does not follow on our current election head");
            return Err(PushError::Orphan);
        }

        // Check (sort of) intrinsic block invariants.
        if let Err(e) = block.verify(self.network_id) {
            warn!("Rejecting block - verification failed ({:?})", e);
            return Err(PushError::InvalidBlock(e));
        }

        // Check if the block's immediate predecessor is part of the chain.
        let prev_info = self
            .chain_store
            .get_chain_info(
                &macro_block.header.parent_election_hash,
                false,
                Some(&read_txn),
            )
            .ok_or_else(|| {
                warn!("Rejecting block - unknown predecessor");
                PushError::Orphan
            })?;

        // Check the block is the correct election block if it is an election block
        if macro_block.is_election_block()
            && policy::election_block_after(prev_info.head.block_number())
                != macro_block.header.block_number
        {
            warn!(
                "Rejecting block - wrong block number ({:?})",
                macro_block.header.block_number
            );
            return Err(PushError::InvalidSuccessor);
        }

        // Check transactions root
        let hashes: Vec<Blake2bHash> = transactions.iter().map(|tx| tx.hash()).collect();
        let transactions_root = merkle::compute_root_from_hashes::<Blake2bHash>(&hashes);
        if macro_block
            .body
            .as_ref()
            .expect("Missing body!")
            .history_root
            != transactions_root
        {
            warn!("Rejecting block - wrong history root");
            return Err(PushError::InvalidBlock(BlockError::InvalidHistoryRoot));
        }

        // Check Macro Justification
        match macro_block.justification {
            None => {
                warn!("Rejecting block - macro block without justification");
                return Err(PushError::InvalidBlock(BlockError::NoJustification));
            }
            Some(ref justification) => {
                if let Err(_) = justification.verify(
                    macro_block.hash(),
                    &self.current_validators(),
                    policy::TWO_THIRD_SLOTS,
                ) {
                    warn!("Rejecting block - macro block with bad justification");
                    return Err(PushError::InvalidBlock(BlockError::NoJustification));
                }
            }
        }

        // The correct construction of the extrinsics is only checked after the block's inherents are applied.

        // The macro extrinsics must not be None for this function.
        if let Some(ref extrinsics) = macro_block.body {
            let extrinsics_hash: Blake2bHash = extrinsics.hash();
            if extrinsics_hash != macro_block.header.body_root {
                warn!(
                    "Rejecting block - Header extrinsics hash doesn't match real extrinsics hash"
                );
                return Err(PushError::InvalidBlock(BlockError::BodyHashMismatch));
            }
        } else {
            return Err(PushError::InvalidBlock(BlockError::MissingBody));
        }

        // Drop read transaction before creating the write transaction.
        drop(read_txn);

        // We don't know the exact distribution between fork proofs and view changes here.
        // But as this block concludes the epoch, it is not of relevance anymore.
        let chain_info = ChainInfo {
            on_main_chain: false,
            main_chain_successor: None,
            slashed_set: SlashedSet {
                view_change_epoch_state: macro_block.body.as_ref().unwrap().lost_reward_set.clone(),
                fork_proof_epoch_state: Default::default(),
                prev_epoch_state: macro_block.body.as_ref().unwrap().lost_reward_set.clone(),
            },
            head: block,
            cum_tx_fees: transactions.iter().map(|tx| tx.fee).sum(),
        };

        self.extend_isolated_macro(
            chain_info.head.hash(),
            transactions,
            chain_info,
            prev_info,
            push_lock,
        )
    }

    fn extend_isolated_macro(
        &self,
        block_hash: Blake2bHash,
        transactions: &[BlockchainTransaction],
        mut chain_info: ChainInfo,
        mut prev_info: ChainInfo,
        push_lock: MutexGuard<()>,
    ) -> Result<PushResult, PushError> {
        let mut txn = WriteTransaction::new(&self.env);

        let state = self.state.read();

        let macro_block = chain_info.head.unwrap_macro_ref();
        let is_election_block = macro_block.is_election_block();

        // So far, the block is all good.
        // Remove any micro blocks in the current epoch if necessary.
        let blocks_to_revert = self.chain_store.get_blocks_forward(
            &state.macro_head_hash,
            policy::EPOCH_LENGTH - 1,
            true,
            Some(&txn),
        );
        let mut cache_txn = state.transaction_cache.clone();
        for i in (0..blocks_to_revert.len()).rev() {
            let block = &blocks_to_revert[i];
            let micro_block = match block {
                Block::Micro(block) => block,
                _ => {
                    return Err(PushError::BlockchainError(
                        BlockchainError::InconsistentState,
                    ))
                }
            };

            let prev_view_number = if i == 0 {
                state.macro_info.head.view_number()
            } else {
                blocks_to_revert[i - 1].view_number()
            };
            let prev_state_root = if i == 0 {
                &state.macro_info.head.state_root()
            } else {
                blocks_to_revert[i - 1].state_root()
            };

            self.revert_accounts(&state.accounts, &mut txn, micro_block, prev_view_number)?;

            cache_txn.revert_block(block);

            assert_eq!(
                prev_state_root,
                &state.accounts.hash(Some(&txn)),
                "Failed to revert main chain while rebranching - inconsistent state"
            );
        }

        // We cannot verify the slashed set, so we need to trust it here.
        // Also, we verified that macro extrinsics have been set in the corresponding push,
        // thus we can unwrap here.
        let slashed_set = macro_block.body.as_ref().unwrap().disabled_set.clone();
        let current_slashed_set = macro_block.body.as_ref().unwrap().disabled_set.clone();

        // We cannot check the accounts hash yet.
        // Apply transactions and inherents to AccountsTree.
        let slots = state
            .previous_slots
            .as_ref()
            .expect("Slots for last epoch are missing");
        let mut inherents = self.inherents_from_slashed_set(&slashed_set, slots);

        // election blocks finalize the previous epoch.
        if is_election_block {
            inherents.append(&mut self.finalize_previous_epoch(&state, &chain_info));
        }

        // Commit epoch to AccountsTree.
        let receipts = state.accounts.commit(
            &mut txn,
            transactions,
            &inherents,
            macro_block.header.block_number,
        );
        if let Err(e) = receipts {
            warn!("Rejecting block - commit failed: {:?}", e);
            txn.abort();
            #[cfg(feature = "metrics")]
            self.metrics.note_invalid_block();
            return Err(PushError::AccountsError(e));
        }

        drop(state);

        // as with all macro blocks they cannot be rebranched over, so receipts are no longer needed.
        self.chain_store.clear_receipts(&mut txn);

        // Only now can we check macro extrinsics.
        if let Block::Macro(ref mut macro_block) = &mut chain_info.head {
            if is_election_block {
                if let Some(ref validators) =
                    macro_block.body.as_ref().expect("Missing body!").validators
                {
                    let slots = self.next_slots(&macro_block.header.seed, Some(&txn));
                    if &slots.validator_slots != validators {
                        warn!("Rejecting block - Validators don't match real validators");
                        return Err(PushError::InvalidBlock(BlockError::InvalidValidators));
                    }
                } else {
                    warn!("Rejecting block - Validators missing");
                    return Err(PushError::InvalidBlock(BlockError::InvalidValidators));
                }
            }

            let mut computed_extrinsics =
                MacroBody::from_slashed_set(slashed_set, current_slashed_set);

            // The extrinsics must exist for isolated macro blocks, so we can unwrap() here.
            let computed_extrinsics_hash: Blake2bHash = computed_extrinsics.hash();
            if computed_extrinsics_hash != macro_block.header.body_root {
                warn!("Rejecting block - Extrinsics hash doesn't match real extrinsics hash");
                return Err(PushError::InvalidBlock(BlockError::BodyHashMismatch));
            }
        }

        chain_info.on_main_chain = true;
        prev_info.main_chain_successor = Some(chain_info.head.hash());

        self.chain_store
            .put_chain_info(&mut txn, &block_hash, &chain_info, true);
        self.chain_store.put_chain_info(
            &mut txn,
            &chain_info.head.parent_hash(),
            &prev_info,
            false,
        );
        self.chain_store.set_head(&mut txn, &block_hash);
        self.chain_store
            .put_epoch_transactions(&mut txn, &block_hash, transactions);

        // Acquire write lock & commit changes.
        let mut state = self.state.write();
        // FIXME: Macro block sync does not preserve transaction replay protection right now.
        // But this is not an issue in the UTXO model.
        //state.transaction_cache.push_block(&chain_info.head);

        if let Block::Macro(ref macro_block) = chain_info.head {
            state.macro_info = chain_info.clone();
            state.macro_head_hash = block_hash.clone();
            if is_election_block {
                state.election_head = macro_block.clone();
                state.election_head_hash = block_hash.clone();

                let slots = state.current_slots.take().unwrap();
                state.previous_slots.replace(slots);

                let slots = Self::slots_from_block(&macro_block);
                state.current_slots.replace(slots);
            }
        } else {
            unreachable!("Block is not a macro block");
        }

        state.main_chain = chain_info;
        state.head_hash = block_hash.clone();
        state.transaction_cache = cache_txn;
        txn.commit();

        // Give up lock before notifying.
        drop(state);
        drop(push_lock);

        if is_election_block {
            self.notifier
                .read()
                .notify(BlockchainEvent::EpochFinalized(block_hash));
        } else {
            self.notifier
                .read()
                .notify(BlockchainEvent::Finalized(block_hash));
        }
        Ok(PushResult::Extended)
    }

    pub fn contains(&self, hash: &Blake2bHash, include_forks: bool) -> bool {
        match self.chain_store.get_chain_info(hash, false, None) {
            Some(chain_info) => include_forks || chain_info.on_main_chain,
            None => false,
        }
    }

    pub fn get_block_at(&self, height: u32, include_body: bool) -> Option<Block> {
        self.chain_store
            .get_chain_info_at(height, include_body, None)
            .map(|chain_info| chain_info.head)
    }

    pub fn get_block(
        &self,
        hash: &Blake2bHash,
        include_forks: bool,
        include_body: bool,
    ) -> Option<Block> {
        let chain_info_opt = self.chain_store.get_chain_info(hash, include_body, None);
        if let Some(chain_info) = chain_info_opt {
            if chain_info.on_main_chain || include_forks {
                return Some(chain_info.head);
            }
        }
        None
    }

    pub fn get_blocks(
        &self,
        start_block_hash: &Blake2bHash,
        count: u32,
        include_body: bool,
        direction: Direction,
    ) -> Vec<Block> {
        self.chain_store
            .get_blocks(start_block_hash, count, include_body, direction, None)
    }

    // gets transactions for either an epoch or a batch.
    fn get_transactions(
        &self,
        batch_or_epoch_index: u32,
        for_batch: bool,
        txn_option: Option<&Transaction>,
    ) -> Option<Vec<BlockchainTransaction>> {
        if !for_batch {
            // It might be that we synced this epoch via macro block sync.
            // Therefore, we check this first.
            let macro_block = policy::macro_block_of(batch_or_epoch_index);
            if let Some(macro_block_info) =
                self.chain_store
                    .get_chain_info_at(macro_block, false, txn_option)
            {
                if let Some(txs) = self
                    .chain_store
                    .get_epoch_transactions(&macro_block_info.head.hash(), txn_option)
                {
                    return Some(txs);
                }
            }
        }

        // Else retrieve transactions normally from micro blocks.
        // first_block_of(_batch) is guaranteed to return a micro block!!!
        let first_block = if for_batch {
            policy::first_block_of_batch(batch_or_epoch_index)
        } else {
            policy::first_block_of(batch_or_epoch_index)
        };
        let first_block = self
            .chain_store
            .get_block_at(first_block, true, txn_option)
            .or_else(|| {
                debug!(
                    "get_block_at didn't return first block of {}: block_height={}",
                    if for_batch { "batch" } else { "epoch" },
                    first_block,
                );
                None
            })?;

        let first_hash = first_block.hash();
        let mut txs = first_block.unwrap_micro().body.unwrap().transactions;

        // Excludes current block and macro block.
        let blocks = self.chain_store.get_blocks(
            &first_hash,
            if for_batch {
                policy::BATCH_LENGTH
            } else {
                policy::EPOCH_LENGTH
            } - 2,
            true,
            Direction::Forward,
            txn_option,
        );
        // We need to make sure that we have all micro blocks.
        if blocks.len() as u32
            != if for_batch {
                policy::BATCH_LENGTH
            } else {
                policy::EPOCH_LENGTH
            } - 2
        {
            debug!(
                "Exptected {} blocks, but get_blocks returned {}",
                if for_batch {
                    policy::BATCH_LENGTH
                } else {
                    policy::EPOCH_LENGTH
                } - 2,
                blocks.len()
            );
            for block in &blocks {
                debug!("Returned block {} - {}", block.block_number(), block.hash());
            }
            return None;
        }

        txs.extend(
            blocks
                .into_iter()
                // blocks need to be filtered as Block::unwrap_transacitons makes use of
                // Block::unwrap_micro, which panics for non micro blocks.
                .filter(Block::is_micro as fn(&_) -> _)
                .map(Block::unwrap_transactions as fn(_) -> _)
                .flatten(),
        );

        Some(txs)
    }

    pub fn get_batch_transactions(
        &self,
        batch: u32,
        txn_option: Option<&Transaction>,
    ) -> Option<Vec<BlockchainTransaction>> {
        self.get_transactions(batch, true, txn_option)
    }

    pub fn get_epoch_transactions(
        &self,
        epoch: u32,
        txn_option: Option<&Transaction>,
    ) -> Option<Vec<BlockchainTransaction>> {
        self.get_transactions(epoch, false, txn_option)
    }

    pub fn get_history_root(
        &self,
        batch: u32,
        txn_option: Option<&Transaction>,
    ) -> Option<Blake2bHash> {
        let hashes: Vec<Blake2bHash> = self
            .get_batch_transactions(batch, txn_option)?
            .iter()
            .map(|tx| tx.hash())
            .collect(); // BlockchainTransaction::hash does *not* work here.

        Some(merkle::compute_root_from_hashes::<Blake2bHash>(&hashes))
    }

    pub fn height(&self) -> u32 {
        self.block_number()
    }

    pub fn block_number(&self) -> u32 {
        self.state.read_recursive().main_chain.head.block_number()
    }

    pub fn next_view_number(&self) -> u32 {
        self.state
            .read_recursive()
            .main_chain
            .head
            .next_view_number()
    }

    pub fn view_number(&self) -> u32 {
        self.state.read_recursive().main_chain.head.view_number()
    }

    pub fn head(&self) -> MappedRwLockReadGuard<Block> {
        let guard = self.state.read();
        RwLockReadGuard::map(guard, |s| &s.main_chain.head)
    }

    pub fn head_hash(&self) -> Blake2bHash {
        self.state.read().head_hash.clone()
    }

    pub fn macro_head(&self) -> MappedRwLockReadGuard<MacroBlock> {
        let guard = self.state.read();
        RwLockReadGuard::map(guard, |s| s.macro_info.head.unwrap_macro_ref())
    }

    pub fn macro_head_hash(&self) -> Blake2bHash {
        self.state.read().macro_head_hash.clone()
    }

    pub fn election_head(&self) -> MappedRwLockReadGuard<MacroBlock> {
        let guard = self.state.read();
        RwLockReadGuard::map(guard, |s| &s.election_head)
    }

    pub fn election_head_hash(&self) -> Blake2bHash {
        self.state.read().election_head_hash.clone()
    }

    pub fn get_slot_for_next_block(
        &self,
        view_number: u32,
        txn_option: Option<&Transaction>,
    ) -> (Slot, u16) {
        let block_number = self.height() + 1;
        self.get_slot_at(block_number, view_number, txn_option)
            .unwrap()
    }

    pub fn next_slots(&self, seed: &VrfSeed, txn_option: Option<&Transaction>) -> Slots {
        let validator_registry = NetworkInfo::from_network_id(self.network_id)
            .validator_registry_address()
            .expect("No ValidatorRegistry");
        let staking_account = self
            .state
            .read()
            .accounts()
            .get(validator_registry, txn_option);
        if let Account::Staking(ref staking_contract) = staking_account {
            return staking_contract.select_validators(seed);
        }
        panic!("Account at validator registry address is not the stacking contract!");
    }

    pub fn next_validators(&self, seed: &VrfSeed, txn: Option<&Transaction>) -> ValidatorSlots {
        self.next_slots(seed, txn).into()
    }

    pub fn get_next_block_type(&self, last_number: Option<u32>) -> BlockType {
        let last_block_number = match last_number {
            None => self.head().block_number(),
            Some(n) => n,
        };

        if policy::is_macro_block_at(last_block_number + 1) {
            BlockType::Macro
        } else {
            BlockType::Micro
        }
    }

    pub fn get_slot_at(
        &self,
        block_number: u32,
        view_number: u32,
        txn_option: Option<&Transaction>,
    ) -> Option<(Slot, u16)> {
        let state = self.state.read_recursive();

        let read_txn;
        let txn = if let Some(txn) = txn_option {
            txn
        } else {
            read_txn = ReadTransaction::new(&self.env);
            &read_txn
        };

        // Gets slots collection from either the cached ones, or from the election block.
        // Note: We need to handle the case where `state.block_number()` is at a macro block (so `state.current_slots`
        // was already updated by it, pushing this epoch's slots to `state.previous_slots` and deleting previous'
        // epoch's slots).
        let slots_owned;
        let slots = if policy::epoch_at(state.block_number()) == policy::epoch_at(block_number) {
            if policy::is_election_block_at(state.block_number()) {
                state.previous_slots.as_ref().unwrap_or_else(|| {
                    panic!(
                        "Missing epoch's slots for block {}.{}",
                        block_number, view_number
                    )
                })
            } else {
                state
                    .current_slots
                    .as_ref()
                    .expect("Missing current epoch's slots")
            }
        } else if !policy::is_election_block_at(state.block_number())
            && policy::epoch_at(state.block_number()) == policy::epoch_at(block_number) + 1
        {
            state.previous_slots.as_ref().unwrap_or_else(|| {
                panic!(
                    "Missing previous epoch's slots for block {}.{}",
                    block_number, view_number
                )
            })
        } else {
            let macro_block = self
                .chain_store
                .get_block_at(
                    policy::election_block_before(block_number),
                    true,
                    Some(&txn),
                )?
                .unwrap_macro();

            // Get slots of epoch
            slots_owned = macro_block.try_into().unwrap();
            &slots_owned
        };

        let prev_info = self
            .chain_store
            .get_chain_info_at(block_number - 1, false, Some(&txn))?;

        Some(get_slot_at(block_number, view_number, &prev_info, slots))
    }

    pub fn state(&self) -> RwLockReadGuard<BlockchainState> {
        self.state.read()
    }

    pub fn create_slash_inherents(
        &self,
        fork_proofs: &[ForkProof],
        view_changes: &Option<ViewChanges>,
        txn_option: Option<&Transaction>,
    ) -> Vec<Inherent> {
        let mut inherents = vec![];
        for fork_proof in fork_proofs {
            inherents.push(self.inherent_from_fork_proof(fork_proof, txn_option));
        }
        if let Some(view_changes) = view_changes {
            inherents.append(&mut self.inherents_from_view_changes(view_changes, txn_option));
        }

        inherents
    }

    /// Expects a *verified* proof!
    pub fn inherent_from_fork_proof(
        &self,
        fork_proof: &ForkProof,
        txn_option: Option<&Transaction>,
    ) -> Inherent {
        let (producer, slot) = self
            .get_slot_at(
                fork_proof.header1.block_number,
                fork_proof.header1.view_number,
                txn_option,
            )
            .unwrap();
        let validator_registry = NetworkInfo::from_network_id(self.network_id)
            .validator_registry_address()
            .expect("No ValidatorRegistry");

        let slot = SlashedSlot {
            slot,
            validator_key: producer.public_key().clone(),
            event_block: fork_proof.header1.block_number,
        };

        Inherent {
            ty: InherentType::Slash,
            target: validator_registry.clone(),
            value: Coin::ZERO,
            data: slot.serialize_to_vec(),
        }
    }

    /// Expects a *verified* proof!
    pub fn inherents_from_view_changes(
        &self,
        view_changes: &ViewChanges,
        txn_option: Option<&Transaction>,
    ) -> Vec<Inherent> {
        let validator_registry = NetworkInfo::from_network_id(self.network_id)
            .validator_registry_address()
            .expect("No ValidatorRegistry");

        (view_changes.first_view_number..view_changes.last_view_number)
            .map(|view_number| {
                let (producer, slot) = self
                    .get_slot_at(view_changes.block_number, view_number, txn_option)
                    .unwrap();
                debug!("Slash inherent: view change: {}", producer.public_key());

                let slot = SlashedSlot {
                    slot,
                    validator_key: producer.public_key().clone(),
                    event_block: view_changes.block_number,
                };

                Inherent {
                    ty: InherentType::Slash,
                    target: validator_registry.clone(),
                    value: Coin::ZERO,
                    data: slot.serialize_to_vec(),
                }
            })
            .collect::<Vec<Inherent>>()
    }

    /// Expects a *verified* proof!
    pub fn inherents_from_slashed_set(&self, slashed_set: &BitSet, slots: &Slots) -> Vec<Inherent> {
        let validator_registry = NetworkInfo::from_network_id(self.network_id)
            .validator_registry_address()
            .expect("No ValidatorRegistry");

        slashed_set
            .iter()
            .map(|slot_number| {
                let producer = slots
                    .get(SlotIndex::Slot(slot_number as u16))
                    .unwrap_or_else(|| panic!("Missing slot for slot number: {}", slot_number));

                let slot = SlashedSlot {
                    slot: slot_number as u16,
                    validator_key: producer.public_key().clone(),
                    event_block: 0,
                };

                Inherent {
                    ty: InherentType::Slash,
                    target: validator_registry.clone(),
                    value: Coin::ZERO,
                    data: slot.serialize_to_vec(),
                }
            })
            .collect::<Vec<Inherent>>()
    }

    /// Get slashed set at specific block number
    /// Returns slash set before applying block with that block_number (TODO Tests)
    pub fn slashed_set_at(&self, block_number: u32) -> Option<SlashedSet> {
        let prev_info = self
            .chain_store
            .get_chain_info_at(block_number - 1, false, None)?;
        Some(prev_info.slashed_set)
    }

    pub fn current_validators(&self) -> MappedRwLockReadGuard<ValidatorSlots> {
        let guard = self.state.read();
        RwLockReadGuard::map(guard, |s| s.current_validators().unwrap())
    }

    pub fn last_validators(&self) -> MappedRwLockReadGuard<ValidatorSlots> {
        let guard = self.state.read();
        RwLockReadGuard::map(guard, |s| s.last_validators().unwrap())
    }

    pub fn finalize_previous_epoch(
        &self,
        state: &BlockchainState,
        chain_info: &ChainInfo,
    ) -> Vec<Inherent> {
        let prev_macro_info = &state.macro_info;
        let macro_header = &chain_info.head.unwrap_macro_ref().header;

        // It might be that we don't have any micro blocks, thus we need to look at the next macro block.
        let epoch = policy::epoch_at(macro_header.block_number) - 1;

        // Special case for first epoch: Epoch 0 is finalized by definition.
        if epoch == 0 {
            return vec![];
        }

        // Get validator slots
        // NOTE: Field `previous_slots` is expected to be always set.
        let validator_slots = &state
            .previous_slots
            .as_ref()
            .expect("Slots for last epoch are missing")
            .validator_slots;

        // Slashed slots (including fork proofs)
        let slashed_set = chain_info.slashed_set.prev_epoch_state.clone();

        // Total reward for the previous epoch
        let block_reward = block_reward_for_epoch(
            chain_info.head.unwrap_macro_ref(),
            prev_macro_info.head.unwrap_macro_ref(),
            self.genesis_supply,
            self.genesis_timestamp,
        );
        let tx_fees = chain_info.cum_tx_fees;
        let reward_pot: Coin = block_reward + tx_fees;

        // Distribute reward between all slots and calculate the remainder
        let slot_reward = reward_pot / policy::SLOTS as u64;
        let remainder = reward_pot % policy::SLOTS as u64;

        // The first slot number of the current validator
        let mut first_slot_number = 0;

        // Peekable iterator to collect slashed slots for validator
        let mut slashed_set_iter = slashed_set.iter().peekable();

        // All accepted inherents.
        let mut inherents = Vec::new();

        // Remember the number of eligible slots that a validator had (that was able to accept the inherent)
        let mut num_eligible_slots_for_accepted_inherent = Vec::new();

        // Remember that the total amount of reward must be burned. The reward for a slot is burned
        // either because the slot was slashed or because the corresponding validator was unable to
        // accept the inherent.
        let mut burned_reward = Coin::ZERO;

        // Compute inherents
        for validator_slot in validator_slots.iter() {
            // The interval of slot numbers for the current slot band is
            // [first_slot_number, last_slot_number). So it actually doesn't include
            // `last_slot_number`.
            let last_slot_number = first_slot_number + validator_slot.num_slots();

            // Compute the number of slashes for this validator slot band.
            let mut num_eligible_slots = validator_slot.num_slots();
            let mut num_slashed_slots = 0;

            while let Some(next_slashed_slot) = slashed_set_iter.peek() {
                let next_slashed_slot = *next_slashed_slot as u16;
                assert!(next_slashed_slot >= first_slot_number);
                if next_slashed_slot < last_slot_number {
                    assert!(num_eligible_slots > 0);
                    num_eligible_slots -= 1;
                    num_slashed_slots += 1;
                } else {
                    break;
                }
            }

            // Compute reward from slot reward and number of eligible slots. Also update the burned
            // reward from the number of slashed slots.
            let reward = slot_reward
                .checked_mul(num_eligible_slots as u64)
                .expect("Overflow in reward");

            burned_reward += slot_reward
                .checked_mul(num_slashed_slots as u64)
                .expect("Overflow in reward");

            // Create inherent for the reward
            let inherent = Inherent {
                ty: InherentType::Reward,
                target: validator_slot.reward_address().clone(),
                value: reward,
                data: vec![],
            };

            // Test whether account will accept inherent. If it can't then the reward will be
            // burned.
            let account = state.accounts.get(&inherent.target, None);

            if account
                .check_inherent(&inherent, macro_header.block_number)
                .is_err()
            {
                debug!(
                    "{} can't accept epoch reward {}",
                    inherent.target, inherent.value
                );
                burned_reward += reward;
            } else {
                num_eligible_slots_for_accepted_inherent.push(num_eligible_slots);
                inherents.push(inherent);
            }

            // Update first_slot_number for next iteration
            first_slot_number = last_slot_number;
        }

        // Check that number of accepted inherents is equal to length of the map that gives us the
        // corresponding number of slots for that staker (which should be equal to the number of
        // validator that will receive rewards).
        assert_eq!(
            inherents.len(),
            num_eligible_slots_for_accepted_inherent.len()
        );

        // Get RNG from last block's seed and build lookup table based on number of eligible slots
        let mut rng = macro_header.seed.rng(VrfUseCase::RewardDistribution, 0);
        let lookup = AliasMethod::new(num_eligible_slots_for_accepted_inherent);

        // Randomly give remainder to one accepting slot. We don't bother to distribute it over all
        // accepting slots because the remainder is always at most SLOTS - 1 Lunas.
        let index = lookup.sample(&mut rng);
        inherents[index].value += remainder;

        // Create the inherent for the burned reward.
        let inherent = Inherent {
            ty: InherentType::Reward,
            target: Address::burn_address(),
            value: burned_reward,
            data: vec![],
        };

        inherents.push(inherent);

        // Push finalize epoch inherent for automatically retiring inactive/malicious validators.
        let validator_registry = NetworkInfo::from_network_id(self.network_id)
            .validator_registry_address()
            .expect("No ValidatorRegistry");

        inherents.push(Inherent {
            ty: InherentType::FinalizeBatch,
            target: validator_registry.clone(),
            value: Coin::ZERO,
            data: Vec::new(),
        });

        inherents
    }

    fn get_macro_locators(&self, max_count: usize, election_blocks_only: bool) -> Vec<Blake2bHash> {
        let mut locators: Vec<Blake2bHash> = Vec::with_capacity(max_count);
        let mut hash = if election_blocks_only {
            self.election_head_hash()
        } else {
            self.macro_head_hash()
        };

        // Push top ten hashes.
        locators.push(hash.clone());
        for _ in 0..10 {
            let block = self.chain_store.get_block(&hash, false, None);
            match block {
                Some(Block::Macro(block)) => {
                    hash = if election_blocks_only {
                        block.header.parent_election_hash
                    } else {
                        block.header.parent_hash
                    };
                    locators.push(hash.clone());
                }
                Some(_) => unreachable!("macro head must be a macro block"),
                None => break,
            }
        }

        let mut step = 2;
        let mut height = if election_blocks_only {
            policy::last_election_block(self.height())
                .saturating_sub((10 + step) * policy::EPOCH_LENGTH)
        } else {
            policy::last_macro_block(self.height())
                .saturating_sub((10 + step) * policy::BATCH_LENGTH)
        };

        let mut opt_block = self.chain_store.get_block_at(height, false, None);
        while let Some(block) = opt_block {
            assert_eq!(block.ty(), BlockType::Macro);
            locators.push(block.header().hash());

            // Respect max count.
            if locators.len() >= max_count {
                break;
            }

            step *= 2;
            height = if election_blocks_only {
                height.saturating_sub(step * policy::EPOCH_LENGTH)
            } else {
                height.saturating_sub(step * policy::BATCH_LENGTH)
            };
            // 0 or underflow means we need to end the loop
            if height == 0 {
                break;
            }

            opt_block = self.chain_store.get_block_at(height, false, None);
        }

        // Push the genesis block hash.
        let genesis_hash = NetworkInfo::from_network_id(self.network_id).genesis_hash();
        if locators.is_empty() || locators.last().unwrap() != genesis_hash {
            // Respect max count, make space for genesis hash if necessary
            if locators.len() >= max_count {
                locators.pop();
            }
            locators.push(genesis_hash.clone());
        }

        locators
    }

    pub fn get_macro_block_locators(&self, max_count: usize) -> Vec<Blake2bHash> {
        self.get_macro_locators(max_count, false)
    }

    pub fn get_election_block_locators(&self, max_count: usize) -> Vec<Blake2bHash> {
        self.get_macro_locators(max_count, true)
    }

    /// Returns None if given start_block_hash is not a macro block.
    pub fn get_macro_blocks(
        &self,
        start_block_hash: &Blake2bHash,
        count: u32,
        include_body: bool,
        direction: Direction,
    ) -> Option<Vec<Block>> {
        self.chain_store.get_macro_blocks(
            start_block_hash,
            count,
            false,
            include_body,
            direction,
            None,
        )
    }

    /// Returns None if given start_block_hash is not a macro block.
    pub fn get_election_blocks(
        &self,
        start_block_hash: &Blake2bHash,
        count: u32,
        include_body: bool,
        direction: Direction,
    ) -> Option<Vec<Block>> {
        self.chain_store.get_macro_blocks(
            start_block_hash,
            count,
            true,
            include_body,
            direction,
            None,
        )
    }

    pub fn write_transaction(&self) -> WriteTransaction {
        WriteTransaction::new(&self.env)
    }
}

impl AbstractBlockchain for Blockchain {
    type Block = Block;

    fn new(
        env: Environment,
        network_id: NetworkId,
        _time: Arc<OffsetTime>,
    ) -> Result<Self, BlockchainError> {
        Blockchain::new(env, network_id)
    }

    #[cfg(feature = "metrics")]
    fn metrics(&self) -> &BlockchainMetrics {
        &self.metrics
    }

    fn network_id(&self) -> NetworkId {
        self.network_id
    }

    fn head_block(&self) -> MappedRwLockReadGuard<Self::Block> {
        self.head()
    }

    fn head_hash(&self) -> Blake2bHash {
        self.head_hash()
    }

    fn head_height(&self) -> u32 {
        self.block_number()
    }

    fn get_block(&self, hash: &Blake2bHash, include_body: bool) -> Option<Self::Block> {
        self.get_block(hash, false, include_body)
    }

    fn get_block_at(&self, height: u32, include_body: bool) -> Option<Self::Block> {
        self.get_block_at(height, include_body)
    }

    fn get_block_locators(&self, max_count: usize) -> Vec<Blake2bHash> {
        let mut locators: Vec<Blake2bHash> = Vec::with_capacity(max_count);
        let mut hash = self.head_hash();

        // Push top ten hashes.
        locators.push(hash.clone());
        for _ in 0..cmp::min(10, self.height()) {
            let block = self.chain_store.get_block(&hash, false, None);
            match block {
                Some(block) => {
                    hash = block.header().parent_hash().clone();
                    locators.push(hash.clone());
                }
                None => break,
            }
        }

        let mut step = 2;
        let mut height = self.height().saturating_sub(10 + step);
        let mut opt_block = self.chain_store.get_block_at(height, false, None);
        while let Some(block) = opt_block {
            locators.push(block.header().hash());

            // Respect max count.
            if locators.len() >= max_count {
                break;
            }

            step *= 2;
            height = match height.checked_sub(step) {
                Some(0) => break, // 0 or underflow means we need to end the loop
                Some(v) => v,
                None => break,
            };

            opt_block = self.chain_store.get_block_at(height, false, None);
        }

        // Push the genesis block hash.
        let genesis_hash = NetworkInfo::from_network_id(self.network_id).genesis_hash();
        if locators.is_empty() || locators.last().unwrap() != genesis_hash {
            // Respect max count, make space for genesis hash if necessary
            if locators.len() >= max_count {
                locators.pop();
            }
            locators.push(genesis_hash.clone());
        }

        locators
    }

    fn get_blocks(
        &self,
        start_block_hash: &Blake2bHash,
        count: u32,
        include_body: bool,
        direction: Direction,
    ) -> Vec<Self::Block> {
        self.get_blocks(start_block_hash, count, include_body, direction)
    }

    fn push(&self, block: Self::Block) -> Result<PushResult, PushError> {
        self.push(block)
    }

    fn contains(&self, hash: &Blake2bHash, include_forks: bool) -> bool {
        self.contains(hash, include_forks)
    }

    #[allow(unused_variables)]
    fn get_accounts_proof(
        &self,
        block_hash: &Blake2bHash,
        addresses: &[Address],
    ) -> Option<AccountsProof<Account>> {
        unimplemented!()
    }

    #[allow(unused_variables)]
    fn get_transactions_proof(
        &self,
        block_hash: &Blake2bHash,
        addresses: &HashSet<Address>,
    ) -> Option<TransactionsProof> {
        unimplemented!()
    }

    #[allow(unused_variables)]
    fn get_transaction_receipts_by_address(
        &self,
        address: &Address,
        sender_limit: usize,
        recipient_limit: usize,
    ) -> Vec<TransactionReceipt> {
        unimplemented!()
    }

    fn register_listener<T: Listener<BlockchainEvent> + 'static>(
        &self,
        listener: T,
    ) -> ListenerHandle {
        self.notifier.write().register(listener)
    }

    fn lock(&self) -> MutexGuard<()> {
        self.push_lock.lock()
    }

    fn get_account(&self, address: &Address) -> Account {
        self.state.read().accounts.get(address, None)
    }

    fn contains_tx_in_validity_window(&self, tx_hash: &Blake2bHash) -> bool {
        self.state.read().transaction_cache.contains(tx_hash)
    }

    #[allow(unused_variables)]
    fn head_hash_from_store(&self, txn: &ReadTransaction) -> Option<Blake2bHash> {
        unimplemented!()
    }

    fn get_accounts_chunk(
        &self,
        prefix: &str,
        size: usize,
        txn_option: Option<&Transaction>,
    ) -> Option<AccountsTreeChunk<Account>> {
        self.state
            .read()
            .accounts
            .get_chunk(prefix, size, txn_option)
    }

    fn get_epoch_transactions(
        &self,
        epoch: u32,
        txn_option: Option<&Transaction>,
    ) -> Option<Vec<BlockchainTransaction>> {
        Blockchain::get_epoch_transactions(self, epoch, txn_option)
    }

    fn get_batch_transactions(
        &self,
        batch: u32,
        txn_option: Option<&Transaction>,
    ) -> Option<Vec<BlockchainTransaction>> {
        Blockchain::get_batch_transactions(self, batch, txn_option)
    }

    fn validator_registry_address(&self) -> Option<&Address> {
        NetworkInfo::from_network_id(self.network_id).validator_registry_address()
    }
}
