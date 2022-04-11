use crate::{BlsKeyPair, SchnorrKeyPair};
use nimiq_account::Inherent;
use nimiq_block::{
    Block, ForkProof, MacroBlock, MacroBody, MacroHeader, MicroBlock, MicroBody, MicroHeader,
    MicroJustification, MultiSignature, SignedViewChange, TendermintIdentifier, TendermintProof,
    TendermintProposal, TendermintStep, TendermintVote, ViewChange, ViewChangeProof, ViewChanges,
};
use nimiq_blockchain::{AbstractBlockchain, Blockchain, ExtendedTransaction};
use nimiq_bls::AggregateSignature;
use nimiq_collections::BitSet;
use nimiq_hash::{Blake2bHash, Blake2sHash, Hash};
use nimiq_primitives::policy;
use nimiq_transaction::Transaction;
use nimiq_vrf::{VrfEntropy, VrfSeed};

#[derive(Clone, Default)]
pub struct BlockConfig {
    pub version: Option<u16>,
    pub block_number_offset: i32,
    pub timestamp_offset: i64,
    pub parent_hash: Option<Blake2bHash>,
    pub seed: Option<VrfSeed>,
    pub missing_body: bool,
    pub body_hash: Option<Blake2bHash>,
    pub state_root: Option<Blake2bHash>,
    pub history_root: Option<Blake2bHash>,
    pub view_number_offset: i32,

    pub omit_view_change_proof: bool,
    pub view_change_block_number_offset: i32,
    pub view_change_view_number_offset: i32,
    pub view_change_vrf_entropy: Option<VrfEntropy>,

    // Micro only
    pub micro_only: bool,
    pub view_change_proof: Option<ViewChangeProof>,
    pub fork_proofs: Vec<ForkProof>,
    pub transactions: Vec<Transaction>,
    pub extra_data: Vec<u8>,

    // Macro only
    pub macro_only: bool,
    pub parent_election_hash: Option<Blake2bHash>,
    pub tendermint_round: Option<u32>,
}

/// `config` can be used to generate blocks that can be invalid in some way. config == Default creates a valid block.
pub fn next_micro_block(
    signing_key: &SchnorrKeyPair,
    voting_key: &BlsKeyPair,
    blockchain: &Blockchain,
    config: &BlockConfig,
) -> MicroBlock {
    let block_number = (blockchain.block_number() as i32 + 1 + config.block_number_offset) as u32;

    let timestamp = (blockchain.head().timestamp() as i64 + 1 + config.timestamp_offset) as u64;

    let parent_hash = config
        .parent_hash
        .clone()
        .unwrap_or_else(|| blockchain.head_hash());

    let prev_seed = blockchain.head().seed().clone();
    let seed = config
        .seed
        .clone()
        .unwrap_or_else(|| prev_seed.sign_next(signing_key));

    let view_number = (blockchain.next_view_number() as i32 + config.view_number_offset) as u32;

    let mut transactions = config.transactions.clone();
    transactions.sort_unstable();

    let view_changes = ViewChanges::new(
        block_number,
        blockchain.next_view_number(),
        view_number,
        prev_seed.entropy(),
    );

    let inherents = blockchain.create_slash_inherents(&config.fork_proofs, &view_changes, None);

    let state_root = config.state_root.clone().unwrap_or_else(|| {
        blockchain
            .state()
            .accounts
            .get_root_with(&transactions, &inherents, block_number, timestamp)
            .expect("Failed to compute accounts hash during block production")
    });

    let ext_txs = ExtendedTransaction::from(
        blockchain.network_id,
        block_number,
        timestamp,
        transactions,
        inherents,
    );

    let mut txn = blockchain.write_transaction();

    let history_root = config.history_root.clone().unwrap_or_else(|| {
        blockchain
            .history_store
            .add_to_history(&mut txn, policy::epoch_at(block_number), &ext_txs)
            .expect("Failed to compute history root during block production.")
    });

    txn.abort();

    let body = MicroBody {
        fork_proofs: config.fork_proofs.clone(),
        transactions: config.transactions.clone(),
    };

    let header = MicroHeader {
        version: config.version.unwrap_or(policy::VERSION),
        block_number,
        view_number,
        timestamp,
        parent_hash,
        seed,
        extra_data: config.extra_data.clone(),
        state_root,
        body_root: config.body_hash.clone().unwrap_or_else(|| body.hash()),
        history_root,
    };

    let hash = header.hash::<Blake2bHash>();
    let signature = signing_key.sign(hash.as_slice());

    let view_change_proof = if config.view_change_proof.is_some() {
        config.view_change_proof.clone()
    } else if config.view_number_offset <= 0 || config.omit_view_change_proof {
        None
    } else {
        Some(
            config
                .view_change_proof
                .clone()
                .unwrap_or_else(|| create_view_change_proof(voting_key, blockchain, config)),
        )
    };

    MicroBlock {
        header,
        body: if !config.missing_body {
            Some(body)
        } else {
            None
        },
        justification: Some(MicroJustification {
            signature,
            view_change_proof,
        }),
    }
}

fn next_macro_block_proposal(
    signing_key: &SchnorrKeyPair,
    blockchain: &Blockchain,
    config: &BlockConfig,
) -> MacroBlock {
    let block_number = (blockchain.block_number() as i32 + 1 + config.block_number_offset) as u32;

    let timestamp = (blockchain.head().timestamp() as i64 + config.timestamp_offset) as u64;

    let parent_hash = config
        .parent_hash
        .clone()
        .unwrap_or_else(|| blockchain.head_hash());

    let parent_election_hash = config
        .parent_election_hash
        .clone()
        .unwrap_or_else(|| blockchain.election_head_hash());

    let seed = config
        .seed
        .clone()
        .unwrap_or_else(|| blockchain.head().seed().sign_next(signing_key));

    let mut header = MacroHeader {
        version: config.version.unwrap_or(policy::VERSION),
        block_number,
        view_number: 0, // TODO
        timestamp,
        parent_hash,
        parent_election_hash,
        seed,
        extra_data: config.extra_data.clone(),
        state_root: Blake2bHash::default(),
        body_root: Blake2bHash::default(),
        history_root: Blake2bHash::default(),
    };

    let state = blockchain.state();

    let inherents: Vec<Inherent> = blockchain.create_macro_block_inherents(state, &header);

    header.state_root = state
        .accounts
        .get_root_with(&[], &inherents, block_number, timestamp)
        .expect("Failed to compute accounts hash during block production.");

    let ext_txs = ExtendedTransaction::from(
        blockchain.network_id,
        block_number,
        timestamp,
        vec![],
        inherents,
    );

    let mut txn = blockchain.write_transaction();

    header.history_root = blockchain
        .history_store
        .add_to_history(&mut txn, policy::epoch_at(block_number), &ext_txs)
        .expect("Failed to compute history root during block production.");

    txn.abort();

    let disabled_set = blockchain.get_staking_contract().previous_disabled_slots();

    let lost_reward_set = blockchain.get_staking_contract().previous_lost_rewards();

    let validators = if policy::is_election_block_at(blockchain.block_number() + 1) {
        Some(blockchain.next_validators(&header.seed))
    } else {
        None
    };

    let pk_tree_root = validators.as_ref().map(MacroBlock::pk_tree_root);

    let body = MacroBody {
        validators,
        pk_tree_root,
        lost_reward_set,
        disabled_set,
    };

    header.body_root = config.body_hash.clone().unwrap_or_else(|| body.hash());

    MacroBlock {
        header,
        body: Some(body),
        justification: None,
    }
}

pub fn finalize_macro_block(
    voting_key: &BlsKeyPair,
    proposal: TendermintProposal,
    body: MacroBody,
    block_hash: Blake2sHash,
    config: &BlockConfig,
) -> MacroBlock {
    let vote = TendermintVote {
        proposal_hash: Some(block_hash),
        id: TendermintIdentifier {
            block_number: proposal.value.block_number,
            step: TendermintStep::PreCommit,
            round_number: proposal.round,
        },
    };

    let signature = AggregateSignature::from_signatures(&[voting_key
        .secret_key
        .sign(&vote)
        .multiply(policy::SLOTS)]);

    let mut signers = BitSet::new();
    for i in 0..policy::SLOTS {
        signers.insert(i as usize);
    }

    let justification = Some(TendermintProof {
        round: 0,
        sig: MultiSignature::new(signature, signers),
    });

    MacroBlock {
        header: proposal.value,
        justification,
        body: if config.missing_body {
            None
        } else {
            Some(body)
        },
    }
}

pub fn next_macro_block(
    signing_key: &SchnorrKeyPair,
    voting_key: &BlsKeyPair,
    blockchain: &Blockchain,
    config: &BlockConfig,
) -> Block {
    let height = blockchain.block_number() + 1;

    assert!(policy::is_macro_block_at(height));

    let macro_block_proposal = next_macro_block_proposal(signing_key, blockchain, config);

    let block_hash = macro_block_proposal.nano_zkp_hash();

    let validators =
        blockchain.get_validators_for_epoch(policy::epoch_at(blockchain.block_number() + 1), None);
    assert!(validators.is_some());

    Block::Macro(finalize_macro_block(
        voting_key,
        TendermintProposal {
            valid_round: None,
            value: macro_block_proposal.header,
            round: config.tendermint_round.unwrap_or(0),
        },
        macro_block_proposal
            .body
            .or_else(|| Some(MacroBody::default()))
            .unwrap(),
        block_hash,
        config,
    ))
}

fn create_view_change_proof(
    voting_key_pair: &BlsKeyPair,
    blockchain: &Blockchain,
    config: &BlockConfig,
) -> ViewChangeProof {
    let view_change = ViewChange {
        block_number: (blockchain.block_number() as i32
            + 1
            + config.block_number_offset
            + config.view_change_block_number_offset) as u32,
        new_view_number: (blockchain.view_number() as i32
            + config.view_number_offset
            + config.view_change_view_number_offset) as u32,
        vrf_entropy: config
            .view_change_vrf_entropy
            .clone()
            .unwrap_or_else(|| blockchain.head().seed().entropy()),
    };

    let view_change = SignedViewChange::from_message(view_change, &voting_key_pair.secret_key, 0);

    let signature =
        AggregateSignature::from_signatures(&[view_change.signature.multiply(policy::SLOTS)]);
    let mut signers = BitSet::new();
    for i in 0..policy::SLOTS {
        signers.insert(i as usize);
    }

    ViewChangeProof {
        sig: MultiSignature::new(signature, signers),
    }
}