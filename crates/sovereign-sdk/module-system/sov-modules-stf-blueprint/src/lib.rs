#![deny(missing_docs)]
#![doc = include_str!("../README.md")]

use borsh::BorshDeserialize;
use citrea_primitives::fork::{fork_from_block_number, Fork, ForkManager};
use itertools::Itertools;
use rs_merkle::algorithms::Sha256;
use rs_merkle::MerkleTree;
use sov_modules_api::da::BlockHeaderTrait;
use sov_modules_api::hooks::{
    ApplyBlobHooks, ApplySoftConfirmationError, ApplySoftConfirmationHooks, FinalizeHook,
    SlotHooks, TxHooks,
};
use sov_modules_api::{
    native_debug, native_warn, BasicAddress, BlobReaderTrait, Context, DaSpec, DispatchCall,
    Genesis, Signature, Spec, StateCheckpoint, UnsignedSoftConfirmationBatch, WorkingSet, Zkvm,
};
use sov_rollup_interface::da::{DaData, SequencerCommitment};
use sov_rollup_interface::digest::Digest;
use sov_rollup_interface::soft_confirmation::SignedSoftConfirmationBatch;
use sov_rollup_interface::spec::SpecId;
pub use sov_rollup_interface::stf::{BatchReceipt, TransactionReceipt};
use sov_rollup_interface::stf::{SlotResult, StateTransitionFunction};
use sov_rollup_interface::zk::CumulativeStateDiff;
use sov_state::Storage;

mod batch;
mod stf_blueprint;
mod tx_verifier;

pub use batch::Batch;
pub use stf_blueprint::StfBlueprint;
pub use tx_verifier::RawTx;

/// The tx hook for a blueprint runtime
pub struct RuntimeTxHook<C: Context> {
    /// Height to initialize the context
    pub height: u64,
    /// Sequencer public key
    pub sequencer: C::PublicKey,
    /// Current spec
    pub current_spec: SpecId,
}

/// This trait has to be implemented by a runtime in order to be used in `StfBlueprint`.
///
/// The `TxHooks` implementation sets up a transaction context based on the height at which it is
/// to be executed.
pub trait Runtime<C: Context, Da: DaSpec>:
    DispatchCall<Context = C>
    + Genesis<Context = C, Config = Self::GenesisConfig>
    + TxHooks<Context = C, PreArg = RuntimeTxHook<C>, PreResult = C>
    + SlotHooks<Da, Context = C>
    + FinalizeHook<Da, Context = C>
    + ApplySoftConfirmationHooks<
        Da,
        Context = C,
        SoftConfirmationResult = SequencerOutcome<
            <<Da as DaSpec>::BlobTransaction as BlobReaderTrait>::Address,
        >,
    > + ApplyBlobHooks<
        Da::BlobTransaction,
        Context = C,
        BlobResult = SequencerOutcome<
            <<Da as DaSpec>::BlobTransaction as BlobReaderTrait>::Address,
        >,
    > + Default
{
    /// GenesisConfig type.
    type GenesisConfig: Send + Sync;

    #[cfg(feature = "native")]
    /// GenesisPaths type.
    type GenesisPaths: Send + Sync;

    #[cfg(feature = "native")]
    /// Default rpc methods.
    fn rpc_methods(storage: <C as Spec>::Storage) -> jsonrpsee::RpcModule<()>;

    #[cfg(feature = "native")]
    /// Reads genesis configs.
    fn genesis_config(
        genesis_paths: &Self::GenesisPaths,
    ) -> Result<Self::GenesisConfig, anyhow::Error>;
}

/// The receipts of all the transactions in a batch.
#[derive(Debug, Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TxEffect {
    /// Batch was reverted.
    Reverted,
    /// Batch was processed successfully.
    Successful,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
/// Represents the different outcomes that can occur for a sequencer after batch processing.
pub enum SequencerOutcome<A: BasicAddress> {
    /// Sequencer receives reward amount in defined token and can withdraw its deposit
    Rewarded(u64),
    /// Sequencer loses its deposit and receives no reward
    Slashed {
        /// Reason why sequencer was slashed.
        reason: SlashingReason,
        #[serde(bound(deserialize = ""))]
        /// Sequencer address on DA.
        sequencer_da_address: A,
    },
    /// Batch was ignored, sequencer deposit left untouched.
    Ignored,
}

/// Genesis parameters for a blueprint
pub struct GenesisParams<RT> {
    /// The runtime genesis parameters
    pub runtime: RT,
}

/// Reason why sequencer was slashed.
#[derive(Debug, Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SlashingReason {
    /// This status indicates problem with batch deserialization.
    InvalidBatchEncoding,
    /// Stateless verification failed, for example deserialized transactions have invalid signatures.
    StatelessVerificationFailed,
    /// This status indicates problem with transaction deserialization.
    InvalidTransactionEncoding,
}

/// Trait for soft confirmation handling
pub trait StfBlueprintTrait<C: Context, Da: DaSpec, Vm: Zkvm>:
    StateTransitionFunction<Vm, Da>
{
    /// Begin a soft confirmation
    #[allow(clippy::too_many_arguments)]
    fn begin_soft_confirmation(
        &self,
        current_spec: SpecId,
        sequencer_public_key: &[u8],
        pre_state_root: &Self::StateRoot,
        pre_state: Self::PreState,
        witness: <<C as Spec>::Storage as Storage>::Witness,
        slot_header: &<Da as DaSpec>::BlockHeader,
        soft_confirmation: &mut SignedSoftConfirmationBatch,
    ) -> (Result<(), ApplySoftConfirmationError>, WorkingSet<C>);

    /// Apply soft confirmation transactions
    fn apply_soft_confirmation_txs(
        &self,
        current_spec: SpecId,
        txs: Vec<Vec<u8>>,
        batch_workspace: WorkingSet<C>,
    ) -> (WorkingSet<C>, Vec<TransactionReceipt<TxEffect>>);

    /// End a soft confirmation
    fn end_soft_confirmation(
        &self,
        current_spec: SpecId,
        sequencer_public_key: &[u8],
        soft_confirmation: &mut SignedSoftConfirmationBatch,
        tx_receipts: Vec<TransactionReceipt<TxEffect>>,
        batch_workspace: WorkingSet<C>,
    ) -> (BatchReceipt<(), TxEffect>, StateCheckpoint<C>);

    /// Finalizes a soft confirmation
    fn finalize_soft_confirmation(
        &self,
        current_spec: SpecId,
        batch_receipt: BatchReceipt<(), TxEffect>,
        checkpoint: StateCheckpoint<C>,
        pre_state: Self::PreState,
        soft_confirmation: &mut SignedSoftConfirmationBatch,
    ) -> SlotResult<
        Self::StateRoot,
        Self::ChangeSet,
        Self::BatchReceiptContents,
        Self::TxReceiptContents,
        Self::Witness,
    >;
}

impl<C, RT, Vm, Da> StfBlueprintTrait<C, Da, Vm> for StfBlueprint<C, Da, Vm, RT>
where
    C: Context,
    Vm: Zkvm,
    Da: DaSpec,
    RT: Runtime<C, Da>,
{
    fn begin_soft_confirmation(
        &self,
        current_spec: SpecId,
        sequencer_public_key: &[u8],
        pre_state_root: &Self::StateRoot,
        pre_state: <C>::Storage,
        witness: <<C as Spec>::Storage as Storage>::Witness,
        slot_header: &<Da as DaSpec>::BlockHeader,
        soft_confirmation: &mut SignedSoftConfirmationBatch,
    ) -> (Result<(), ApplySoftConfirmationError>, WorkingSet<C>) {
        native_debug!("Applying soft confirmation in STF Blueprint");

        // check if soft confirmation is coming from our sequencer
        assert_eq!(
            soft_confirmation.sequencer_pub_key(),
            sequencer_public_key,
            "Sequencer public key must match"
        );

        // then verify da hashes match
        assert_eq!(
            soft_confirmation.da_slot_hash(),
            slot_header.hash().into(),
            "DA slot hashes must match"
        );

        // then verify da transactions commitment match
        assert_eq!(
            soft_confirmation.da_slot_txs_commitment(),
            slot_header.txs_commitment().into(),
            "DA slot hashes must match"
        );

        let checkpoint = StateCheckpoint::with_witness(pre_state, witness);

        self.begin_soft_confirmation_inner(
            checkpoint,
            soft_confirmation,
            pre_state_root,
            current_spec,
        )
    }

    fn apply_soft_confirmation_txs(
        &self,
        current_spec: SpecId,
        txs: Vec<Vec<u8>>,
        batch_workspace: WorkingSet<C>,
    ) -> (WorkingSet<C>, Vec<TransactionReceipt<TxEffect>>) {
        self.apply_sov_txs_inner(txs, current_spec, batch_workspace)
    }

    fn end_soft_confirmation(
        &self,
        _current_spec: SpecId,
        sequencer_public_key: &[u8],
        soft_confirmation: &mut SignedSoftConfirmationBatch,
        tx_receipts: Vec<TransactionReceipt<TxEffect>>,
        batch_workspace: WorkingSet<C>,
    ) -> (BatchReceipt<(), TxEffect>, StateCheckpoint<C>) {
        let unsigned = UnsignedSoftConfirmationBatch::new(
            soft_confirmation.da_slot_height(),
            soft_confirmation.da_slot_hash(),
            soft_confirmation.da_slot_txs_commitment(),
            soft_confirmation.txs(),
            soft_confirmation.deposit_data(),
            soft_confirmation.l1_fee_rate(),
            soft_confirmation.timestamp(),
        );

        let unsigned_raw = borsh::to_vec(&unsigned).unwrap();

        // check the claimed hash
        assert_eq!(
            soft_confirmation.hash(),
            Into::<[u8; 32]>::into(<C as Spec>::Hasher::digest(unsigned_raw)),
            "Soft confirmation hashes must match"
        );

        // verify signature
        assert!(
            verify_soft_confirmation_signature::<C>(
                unsigned,
                soft_confirmation.signature().as_slice(),
                sequencer_public_key
            )
            .is_ok(),
            "Signature verification must succeed"
        );

        let (apply_soft_confirmation_result, checkpoint) =
            self.end_soft_confirmation_inner(soft_confirmation, tx_receipts, batch_workspace);

        (apply_soft_confirmation_result.unwrap(), checkpoint)
    }

    fn finalize_soft_confirmation(
        &self,
        _current_spec: SpecId,
        batch_receipt: BatchReceipt<(), TxEffect>,
        checkpoint: StateCheckpoint<C>,
        pre_state: Self::PreState,
        soft_confirmation: &mut SignedSoftConfirmationBatch,
    ) -> SlotResult<
        <C::Storage as Storage>::Root,
        C::Storage,
        (),
        TxEffect,
        <<C as Spec>::Storage as Storage>::Witness,
    > {
        native_debug!(
            "soft confirmation with hash: {:?} from sequencer {:?} has been applied with #{} transactions.",
            soft_confirmation.hash(),
            soft_confirmation.sequencer_pub_key(),
            batch_receipt.tx_receipts.len(),
        );

        let mut batch_receipts = vec![];

        for (i, tx_receipt) in batch_receipt.tx_receipts.iter().enumerate() {
            native_debug!(
                "tx #{} hash: 0x{} result {:?}",
                i,
                hex::encode(tx_receipt.tx_hash),
                tx_receipt.receipt
            );
        }
        batch_receipts.push(batch_receipt);

        let (state_root, witness, storage, state_diff) = {
            let working_set = checkpoint.to_revertable();
            // Save checkpoint
            let mut checkpoint = working_set.checkpoint();

            let (cache_log, mut witness) = checkpoint.freeze();

            let (root_hash, state_update, state_diff) = pre_state
                .compute_state_update(cache_log, &mut witness)
                .expect("jellyfish merkle tree update must succeed");

            let mut working_set = checkpoint.to_revertable();

            self.runtime
                .finalize_hook(&root_hash, &mut working_set.accessory_state());

            let mut checkpoint = working_set.checkpoint();
            let accessory_log = checkpoint.freeze_non_provable();

            pre_state.commit(&state_update, &accessory_log);

            (root_hash, witness, pre_state, state_diff)
        };

        SlotResult {
            state_root,
            change_set: storage,
            batch_receipts,
            witness,
            state_diff,
        }
    }
}

impl<C, RT, Vm, Da> StateTransitionFunction<Vm, Da> for StfBlueprint<C, Da, Vm, RT>
where
    C: Context,
    Da: DaSpec,
    Vm: Zkvm,
    RT: Runtime<C, Da>,
{
    type StateRoot = <C::Storage as Storage>::Root;

    type GenesisParams = GenesisParams<<RT as Genesis>::Config>;
    type PreState = C::Storage;
    type ChangeSet = C::Storage;

    type TxReceiptContents = TxEffect;

    type BatchReceiptContents = ();
    // SequencerOutcome<<Da::BlobTransaction as BlobReaderTrait>::Address>;

    type Witness = <<C as Spec>::Storage as Storage>::Witness;

    type Condition = Da::ValidityCondition;

    fn init_chain(
        &self,
        pre_state: Self::PreState,
        params: Self::GenesisParams,
    ) -> (Self::StateRoot, Self::ChangeSet) {
        let mut working_set = StateCheckpoint::new(pre_state.clone()).to_revertable();

        self.runtime
            .genesis(&params.runtime, &mut working_set)
            .expect("Runtime initialization must succeed");

        let mut checkpoint = working_set.checkpoint();
        let (log, mut witness) = checkpoint.freeze();

        let (genesis_hash, state_update, _) = pre_state
            .compute_state_update(log, &mut witness)
            .expect("Storage update must succeed");

        let mut working_set = checkpoint.to_revertable();

        self.runtime
            .finalize_hook(&genesis_hash, &mut working_set.accessory_state());

        let accessory_log = working_set.checkpoint().freeze_non_provable();

        // TODO: Commit here for now, but probably this can be done outside of STF
        // TODO: Commit is fine
        pre_state.commit(&state_update, &accessory_log);

        (genesis_hash, pre_state)
    }

    fn apply_slot<'a, I>(
        &self,
        _current_spec: SpecId,
        _pre_state_root: &Self::StateRoot,
        _pre_state: Self::PreState,
        _witness: Self::Witness,
        _slot_header: &Da::BlockHeader,
        _validity_condition: &Da::ValidityCondition,
        _blobs: I,
    ) -> SlotResult<
        Self::StateRoot,
        Self::ChangeSet,
        Self::BatchReceiptContents,
        Self::TxReceiptContents,
        Self::Witness,
    >
    where
        I: IntoIterator<Item = &'a mut Da::BlobTransaction>,
    {
        unimplemented!();
    }

    fn apply_soft_confirmation(
        &self,
        current_spec: SpecId,
        sequencer_public_key: &[u8],
        pre_state_root: &Self::StateRoot,
        pre_state: Self::PreState,
        witness: Self::Witness,
        slot_header: &<Da as DaSpec>::BlockHeader,
        _validity_condition: &<Da as DaSpec>::ValidityCondition,
        soft_confirmation: &mut SignedSoftConfirmationBatch,
    ) -> SlotResult<
        Self::StateRoot,
        Self::ChangeSet,
        Self::BatchReceiptContents,
        Self::TxReceiptContents,
        Self::Witness,
    > {
        match self.begin_soft_confirmation(
            current_spec,
            sequencer_public_key,
            pre_state_root,
            pre_state.clone(),
            witness,
            slot_header,
            soft_confirmation,
        ) {
            (Ok(()), batch_workspace) => {
                let (batch_workspace, tx_receipts) = self.apply_soft_confirmation_txs(
                    current_spec,
                    soft_confirmation.txs(),
                    batch_workspace,
                );

                let (batch_receipt, checkpoint) = self.end_soft_confirmation(
                    current_spec,
                    sequencer_public_key,
                    soft_confirmation,
                    tx_receipts,
                    batch_workspace,
                );

                self.finalize_soft_confirmation(
                    current_spec,
                    batch_receipt,
                    checkpoint,
                    pre_state,
                    soft_confirmation,
                )
            }
            (Err(err), batch_workspace) => {
                native_warn!(
                    "Error applying soft confirmation: {:?} \n reverting batch workspace",
                    err
                );
                batch_workspace.revert();
                SlotResult {
                    state_root: pre_state_root.clone(),
                    change_set: pre_state, // should be empty
                    batch_receipts: vec![],
                    witness: <<C as Spec>::Storage as Storage>::Witness::default(),
                    state_diff: vec![],
                }
            }
        }
    }

    fn apply_soft_confirmations_from_sequencer_commitments(
        &self,
        sequencer_public_key: &[u8],
        sequencer_da_public_key: &[u8],
        initial_state_root: &Self::StateRoot,
        initial_batch_hash: [u8; 32],
        pre_state: Self::PreState,
        da_data: Vec<<Da as DaSpec>::BlobTransaction>,
        sequencer_commitments_range: (u32, u32),
        witnesses: std::collections::VecDeque<Vec<Self::Witness>>,
        slot_headers: std::collections::VecDeque<Vec<<Da as DaSpec>::BlockHeader>>,
        validity_condition: &<Da as DaSpec>::ValidityCondition,
        soft_confirmations: std::collections::VecDeque<Vec<SignedSoftConfirmationBatch>>,
        forks: Vec<(SpecId, u64)>,
    ) -> (Self::StateRoot, CumulativeStateDiff) {
        let mut state_diff = CumulativeStateDiff::default();

        // First extract all sequencer commitments
        // Ignore broken DaData and zk proofs. Also ignore ForcedTransaction's (will be implemented in the future).
        let mut sequencer_commitments: Vec<SequencerCommitment> = vec![];
        for blob in da_data {
            // TODO: get sequencer da pub key
            if blob.sender().as_ref() == sequencer_da_public_key {
                let da_data = DaData::try_from_slice(blob.verified_data());

                if let Ok(DaData::SequencerCommitment(commitment)) = da_data {
                    sequencer_commitments.push(commitment);
                }
            }
        }

        // Sort commitments just in case
        sequencer_commitments.sort_unstable();

        // Then verify these soft confirmations.

        let mut current_state_root = initial_state_root.clone();
        let mut previous_batch_hash = initial_batch_hash;
        let mut last_commitment_end_height: Option<u64> = None;

        // should panic if number of sequencer commitments, soft confirmations, slot headers and witnesses don't match
        for (((sequencer_commitment, soft_confirmations), da_block_headers), witnesses) in
            sequencer_commitments
                .into_iter()
                .skip(sequencer_commitments_range.0 as usize)
                .take(
                    sequencer_commitments_range.1 as usize - sequencer_commitments_range.0 as usize
                        + 1,
                )
                .zip_eq(soft_confirmations)
                .zip_eq(slot_headers)
                .zip_eq(witnesses)
        {
            // if the commitment is not sequential, then the proof is invalid.
            if let Some(end_height) = last_commitment_end_height {
                assert_eq!(
                    end_height + 1,
                    sequencer_commitment.l2_start_block_number,
                    "Sequencer commitments must be sequential"
                );

                last_commitment_end_height = Some(sequencer_commitment.l2_end_block_number);
            } else {
                last_commitment_end_height = Some(sequencer_commitment.l2_end_block_number);
            }

            // we must verify given DA headers match the commitments
            let mut index_headers = 0;
            let mut index_soft_confirmation = 0;
            let mut current_da_height = da_block_headers[index_headers].height();

            assert_eq!(
                soft_confirmations[index_soft_confirmation].prev_hash(),
                previous_batch_hash,
                "Soft confirmation previous hash must match the hash of the block before"
            );

            assert_eq!(
                soft_confirmations[index_soft_confirmation].da_slot_hash(),
                da_block_headers[index_headers].hash().into(),
                "Soft confirmation DA slot hash must match DA block header hash"
            );

            assert_eq!(
                soft_confirmations[index_soft_confirmation].da_slot_height(),
                da_block_headers[index_headers].height(),
                "Soft confirmation DA slot height must match DA block header height"
            );

            previous_batch_hash = soft_confirmations[index_soft_confirmation].hash();
            index_soft_confirmation += 1;

            while index_soft_confirmation < soft_confirmations.len() {
                // the soft confirmations DA hash must equal to da hash in index_headers
                // if it's not matching, and if it's not matching the next one, then state transition is invalid.

                if soft_confirmations[index_soft_confirmation].da_slot_hash()
                    == da_block_headers[index_headers].hash().into()
                {
                    assert_eq!(
                        soft_confirmations[index_soft_confirmation].da_slot_height(),
                        da_block_headers[index_headers].height(),
                        "Soft confirmation DA slot height must match DA block header height"
                    );

                    assert_eq!(
                        soft_confirmations[index_soft_confirmation].prev_hash(),
                        previous_batch_hash,
                        "Soft confirmation previous hash must match the hash of the block before"
                    );

                    previous_batch_hash = soft_confirmations[index_soft_confirmation].hash();
                    index_soft_confirmation += 1;
                } else {
                    index_headers += 1;

                    // this can also be done in soft confirmation rule enforcer?
                    assert_eq!(
                        da_block_headers[index_headers].height(),
                        current_da_height + 1,
                        "DA block headers must be in order"
                    );

                    assert_eq!(
                        da_block_headers[index_headers - 1].hash(),
                        da_block_headers[index_headers].prev_hash(),
                        "DA block headers must be in order"
                    );

                    current_da_height += 1;

                    // if the next one is not matching, then the state transition is invalid.
                    assert_eq!(
                        soft_confirmations[index_soft_confirmation].da_slot_hash(),
                        da_block_headers[index_headers].hash().into(),
                        "Soft confirmation DA slot hash must match DA block header hash"
                    );

                    assert_eq!(
                        soft_confirmations[index_soft_confirmation].da_slot_height(),
                        da_block_headers[index_headers].height(),
                        "Soft confirmation DA slot height must match DA block header height"
                    );

                    assert_eq!(
                        soft_confirmations[index_soft_confirmation].prev_hash(),
                        previous_batch_hash,
                        "Soft confirmation previous hash must match the hash of the block before"
                    );

                    previous_batch_hash = soft_confirmations[index_soft_confirmation].hash();
                    index_soft_confirmation += 1;
                }
            }

            // final da header was checked against
            assert_eq!(
                index_headers,
                da_block_headers.len() - 1,
                "All DA headers must be checked"
            );

            // now verify the claimed merkle root of soft confirmation hashes
            let mut soft_confirmation_hashes = vec![];

            for soft_confirmation in soft_confirmations.iter() {
                // given hashes will be checked inside apply_soft_confirmation.
                // so use the claimed hash for now.
                soft_confirmation_hashes.push(soft_confirmation.hash());
            }

            let calculated_root =
                MerkleTree::<Sha256>::from_leaves(soft_confirmation_hashes.as_slice()).root();

            assert_eq!(
                calculated_root,
                Some(sequencer_commitment.merkle_root),
                "Invalid merkle root"
            );

            let mut da_block_headers_iter = da_block_headers.into_iter().peekable();
            let mut da_block_header = da_block_headers_iter.next().unwrap();

            let mut l2_height = sequencer_commitment.l2_start_block_number;
            let mut current_spec = fork_from_block_number(&forks, l2_height);
            let mut fork_manager = ForkManager::new(l2_height, current_spec, forks.clone());

            // now that we verified the claimed root, we can apply the soft confirmations
            // should panic if the number of witnesses and soft confirmations don't match
            for (mut soft_confirmation, witness) in soft_confirmations.into_iter().zip_eq(witnesses)
            {
                if soft_confirmation.da_slot_height() != da_block_header.height() {
                    da_block_header = da_block_headers_iter.next().unwrap();
                }

                let result = self.apply_soft_confirmation(
                    current_spec,
                    sequencer_public_key,
                    &current_state_root,
                    pre_state.clone(),
                    witness,
                    &da_block_header,
                    validity_condition,
                    &mut soft_confirmation,
                );

                current_state_root = result.state_root;
                state_diff.extend(result.state_diff);

                // Notify fork manager about the block so that the next spec / fork
                // is transitioned into if criteria is met.
                if let Err(e) = fork_manager.register_block(l2_height) {
                    panic!("Fork transition failed {}", e);
                }
                l2_height += 1;

                // Update current spec for the next iteration
                current_spec = fork_manager.active_fork();
            }
            assert_eq!(sequencer_commitment.l2_end_block_number, l2_height - 1);
        }

        (current_state_root, state_diff)
    }
}

fn verify_soft_confirmation_signature<C: Context>(
    unsigned_soft_confirmation: UnsignedSoftConfirmationBatch,
    signature: &[u8],
    sequencer_public_key: &[u8],
) -> Result<(), anyhow::Error> {
    let message = borsh::to_vec(&unsigned_soft_confirmation).unwrap();

    let signature = C::Signature::try_from(signature)?;

    // TODO: if verify function is modified to take the claimed hash in signed soft confirmation
    // we wouldn't need to hash the thing twice
    signature.verify(
        &C::PublicKey::try_from(sequencer_public_key)?,
        message.as_slice(),
    )?;

    Ok(())
}
