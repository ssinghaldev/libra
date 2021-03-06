// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    consensus_state::ConsensusState, error::Error,
    persistent_safety_storage::PersistentSafetyStorage, t_safety_rules::TSafetyRules, COUNTERS,
};
use consensus_types::{
    block::Block,
    block_data::BlockData,
    common::{Author, Payload},
    quorum_cert::QuorumCert,
    timeout::Timeout,
    vote::Vote,
    vote_data::VoteData,
    vote_proposal::VoteProposal,
};
use libra_crypto::{ed25519::Ed25519Signature, hash::HashValue};
use libra_logger::debug;
use libra_types::{
    block_info::BlockInfo, epoch_change::EpochChangeProof, ledger_info::LedgerInfo,
    validator_signer::ValidatorSigner, validator_verifier::ValidatorVerifier, waypoint::Waypoint,
};
use std::marker::PhantomData;

/// SafetyRules is responsible for the safety of the consensus:
/// 1) voting rules
/// 2) commit rules
/// 3) ownership of the consensus private key
/// @TODO add a benchmark to evaluate SafetyRules
/// @TODO consider a cache of verified QCs to cut down on verification costs
/// @TODO bootstrap with a hash of a ledger info (waypoint) that includes a validator set
/// @TODO update storage with hash of ledger info (waypoint) during epoch changes (includes a new validator
/// set)
pub struct SafetyRules<T> {
    persistent_storage: PersistentSafetyStorage,
    validator_signer: ValidatorSigner,
    validator_verifier: Option<ValidatorVerifier>,
    marker: PhantomData<T>,
}

impl<T: Payload> SafetyRules<T> {
    /// Constructs a new instance of SafetyRules with the given persistent storage and the
    /// consensus private keys
    /// @TODO replace this with an API that takes in a SafetyRulesConfig
    pub fn new(author: Author, persistent_storage: PersistentSafetyStorage) -> Self {
        let consensus_key = persistent_storage
            .consensus_key()
            .expect("Unable to retrieve consensus private key");
        let validator_signer = ValidatorSigner::new(author, consensus_key);
        Self {
            persistent_storage,
            validator_signer,
            validator_verifier: None,
            marker: PhantomData,
        }
    }

    /// Produces a LedgerInfo that either commits a block based upon the 3-chain commit rule
    /// or an empty LedgerInfo for no commit. The 3-chain commit rule is: B0 (as well as its
    /// prefix) can be committed if there exist certified blocks B1 and B2 that satisfy:
    /// 1) B0 <- B1 <- B2 <--
    /// 2) round(B0) + 1 = round(B1), and
    /// 3) round(B1) + 1 = round(B2).
    pub fn construct_ledger_info(&self, proposed_block: &Block<T>) -> LedgerInfo {
        let block2 = proposed_block.round();
        let block1 = proposed_block.quorum_cert().certified_block().round();
        let block0 = proposed_block.quorum_cert().parent_block().round();

        let commit = block0 + 1 == block1 && block1 + 1 == block2;
        if commit {
            LedgerInfo::new(
                proposed_block.quorum_cert().parent_block().clone(),
                HashValue::zero(),
            )
        } else {
            LedgerInfo::new(BlockInfo::empty(), HashValue::zero())
        }
    }

    /// This verifies a QC makes sense in the current context, specifically that this is for the
    /// current epoch and extends from the preffered round.
    fn verify_qc(&self, qc: &QuorumCert) -> Result<(), Error> {
        let validator_verifier = self
            .validator_verifier
            .as_ref()
            .ok_or(Error::NotInitialized)?;

        qc.verify(validator_verifier)
            .map_err(|e| Error::InvalidQuorumCertificate(e.to_string()))?;

        if qc.parent_block().round() < self.persistent_storage.preferred_round()? {
            Err(Error::InvalidQuorumCertificate(
                "Preferred round too early".into(),
            ))
        } else {
            Ok(())
        }
    }

    /// This sets the current validator verifier and updates the epoch and round information
    /// if this is a new epoch ending ledger info. It also sets the current waypoint to this
    /// LedgerInfo.
    /// @TODO if public key does not match private key in validator set, access persistent storage
    /// to identify new key
    fn start_new_epoch(&mut self, ledger_info: &LedgerInfo) -> Result<(), Error> {
        let epoch_state = ledger_info
            .next_epoch_state()
            .cloned()
            .ok_or(Error::InvalidLedgerInfo)?;
        self.validator_verifier = Some(epoch_state.verifier);
        let current_epoch = self.persistent_storage.epoch()?;

        if current_epoch < epoch_state.epoch {
            // This is ordered specifically to avoid configuration issues:
            // * First set the waypoint to lock in the minimum restarting point,
            // * set the round information,
            // * finally, set the epoch information because once the epoch is set, this `if`
            // statement cannot be re-entered.
            self.persistent_storage
                .set_waypoint(&Waypoint::new_epoch_boundary(ledger_info)?)?;
            self.persistent_storage.set_last_voted_round(0)?;
            self.persistent_storage.set_preferred_round(0)?;
            self.persistent_storage.set_epoch(epoch_state.epoch)?;
        }

        Ok(())
    }

    /// This checks the epoch given against storage for consistent verification
    fn verify_epoch(&self, epoch: u64) -> Result<(), Error> {
        let expected_epoch = self.persistent_storage.epoch()?;
        if epoch != expected_epoch {
            Err(Error::IncorrectEpoch(epoch, expected_epoch))
        } else {
            Ok(())
        }
    }
}

impl<T: Payload> TSafetyRules<T> for SafetyRules<T> {
    fn consensus_state(&mut self) -> Result<ConsensusState, Error> {
        Ok(ConsensusState::new(
            self.persistent_storage.epoch()?,
            self.persistent_storage.last_voted_round()?,
            self.persistent_storage.preferred_round()?,
            self.persistent_storage.waypoint()?,
        ))
    }

    fn initialize(&mut self, proof: &EpochChangeProof) -> Result<(), Error> {
        let waypoint = self.persistent_storage.waypoint()?;
        let last_li = proof
            .verify(&waypoint)
            .map_err(|e| Error::WaypointMismatch(format!("{}", e)))?;
        self.start_new_epoch(last_li.ledger_info())
    }

    /// Verify the QC is correct and up to date, if it is either set the preferred round or start a
    /// new epoch.
    fn update(&mut self, qc: &QuorumCert) -> Result<(), Error> {
        self.verify_qc(qc)?;
        if qc.ends_epoch() {
            self.start_new_epoch(qc.ledger_info().ledger_info())
        } else {
            self.persistent_storage
                .set_preferred_round(qc.parent_block().round())
                .map_err(|e| e.into())
        }
    }

    /// @TODO verify signature on vote proposal
    /// @TODO verify QC correctness
    fn construct_and_sign_vote(&mut self, vote_proposal: &VoteProposal<T>) -> Result<Vote, Error> {
        debug!("Incoming vote proposal to sign.");
        let proposed_block = vote_proposal.block();

        self.verify_epoch(proposed_block.epoch())?;

        let last_voted_round = self.persistent_storage.last_voted_round()?;
        if proposed_block.round() <= last_voted_round {
            debug!(
                "Vote proposal is old {} <= {}",
                proposed_block.round(),
                last_voted_round
            );
            return Err(Error::OldProposal {
                proposal_round: proposed_block.round(),
                last_voted_round: self.persistent_storage.last_voted_round()?,
            });
        }

        let preferred_round = self.persistent_storage.preferred_round()?;
        if proposed_block.quorum_cert().certified_block().round() < preferred_round {
            debug!(
                "Vote proposal certified round is lower than preferred round, {} < {}",
                proposed_block.quorum_cert().certified_block().round(),
                preferred_round,
            );
            return Err(Error::ProposalRoundLowerThenPreferredBlock { preferred_round });
        }

        let new_tree = vote_proposal
            .accumulator_extension_proof()
            .verify(
                proposed_block
                    .quorum_cert()
                    .certified_block()
                    .executed_state_id(),
            )
            .map_err(|e| Error::InvalidAccumulatorExtension {
                error: format!("{}", e),
            })?;

        self.persistent_storage
            .set_last_voted_round(proposed_block.round())?;

        Ok(Vote::new(
            VoteData::new(
                proposed_block.gen_block_info(
                    new_tree.root_hash(),
                    new_tree.version(),
                    vote_proposal.next_epoch_state().cloned(),
                ),
                proposed_block.quorum_cert().certified_block().clone(),
            ),
            self.validator_signer.author(),
            self.construct_ledger_info(proposed_block),
            &self.validator_signer,
        ))
    }

    /// @TODO only sign blocks that are later than last_voted_round and match the current epoch
    /// @TODO verify QC correctness
    /// @TODO verify QC matches preferred round
    fn sign_proposal(&mut self, block_data: BlockData<T>) -> Result<Block<T>, Error> {
        debug!("Incoming proposal to sign.");
        COUNTERS.sign_proposal.inc();
        Ok(Block::new_proposal_from_block_data(
            block_data,
            &self.validator_signer,
        ))
    }

    /// Only sign the timeout if it is greater than or equal to the last_voted_round and ahead of
    /// the preferred_round. We may end up signing timeouts for rounds without first signing votes
    /// if we have received QCs but not proposals. Always map the last_voted_round to the last
    /// signed timeout to prevent equivocation. We can sign the last_voted_round timeout multiple
    /// times by requiring that the underlying signing scheme provides deterministic signatures.
    fn sign_timeout(&mut self, timeout: &Timeout) -> Result<Ed25519Signature, Error> {
        debug!("Incoming timeout message for round {}", timeout.round());
        COUNTERS.requested_sign_timeout.inc();

        self.verify_epoch(timeout.epoch())?;

        let preferred_round = self.persistent_storage.preferred_round()?;
        if timeout.round() <= preferred_round {
            return Err(Error::BadTimeoutPreferredRound(
                timeout.round(),
                preferred_round,
            ));
        }

        let last_voted_round = self.persistent_storage.last_voted_round()?;
        if timeout.round() < last_voted_round {
            return Err(Error::BadTimeoutLastVotedRound(
                timeout.round(),
                last_voted_round,
            ));
        }
        if timeout.round() > last_voted_round {
            self.persistent_storage
                .set_last_voted_round(timeout.round())?;
        }

        let signature = timeout.sign(&self.validator_signer);
        COUNTERS.sign_timeout.inc();
        debug!("Successfully signed timeout message.");
        Ok(signature)
    }
}
