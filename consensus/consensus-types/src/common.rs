// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::proof_of_store::{BatchInfo, ProofOfStore};
use aptos_crypto::HashValue;
use aptos_executor_types::ExecutorResult;
use aptos_infallible::Mutex;
use aptos_logger::prelude::*;
use aptos_types::{
    account_address::AccountAddress, transaction::SignedTransaction,
    validator_verifier::ValidatorVerifier, vm_status::DiscardedVMStatus,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{cmp::min, collections::HashSet, fmt, fmt::Write, sync::Arc};
use tokio::sync::oneshot;

/// The round of a block is a consensus-internal counter, which starts with 0 and increases
/// monotonically. It is used for the protocol safety and liveness (please see the detailed
/// protocol description).
pub type Round = u64;
/// Author refers to the author's account address
pub type Author = AccountAddress;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, Hash, Ord, PartialOrd)]
pub struct TransactionSummary {
    pub sender: AccountAddress,
    pub sequence_number: u64,
}

impl TransactionSummary {
    pub fn new(sender: AccountAddress, sequence_number: u64) -> Self {
        Self {
            sender,
            sequence_number,
        }
    }
}

impl fmt::Display for TransactionSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.sender, self.sequence_number,)
    }
}

#[derive(Clone)]
pub struct TransactionInProgress {
    pub gas_unit_price: u64,
    pub count: u64,
}

impl TransactionInProgress {
    pub fn new(gas_unit_price: u64) -> Self {
        Self {
            gas_unit_price,
            count: 0,
        }
    }

    pub fn gas_unit_price(&self) -> u64 {
        self.gas_unit_price
    }

    pub fn decrement(&mut self) -> u64 {
        self.count -= 1;
        self.count
    }

    pub fn increment(&mut self) -> u64 {
        self.count += 1;
        self.count
    }
}

#[derive(Clone)]
pub struct RejectedTransactionSummary {
    pub sender: AccountAddress,
    pub sequence_number: u64,
    pub hash: HashValue,
    pub reason: DiscardedVMStatus,
}

#[derive(Debug)]
pub enum DataStatus {
    Cached(Vec<SignedTransaction>),
    Requested(
        Vec<(
            HashValue,
            oneshot::Receiver<ExecutorResult<Vec<SignedTransaction>>>,
        )>,
    ),
}

impl DataStatus {
    pub fn extend(&mut self, other: DataStatus) {
        match (self, other) {
            (DataStatus::Requested(v1), DataStatus::Requested(v2)) => v1.extend(v2),
            (_, _) => unreachable!(),
        }
    }

    pub fn take(&mut self) -> DataStatus {
        std::mem::replace(self, DataStatus::Requested(vec![]))
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ProofWithData {
    pub proofs: Vec<ProofOfStore>,
    #[serde(skip)]
    pub status: Arc<Mutex<Option<DataStatus>>>,
}

impl PartialEq for ProofWithData {
    fn eq(&self, other: &Self) -> bool {
        self.proofs == other.proofs && Arc::as_ptr(&self.status) == Arc::as_ptr(&other.status)
    }
}

impl Eq for ProofWithData {}

impl ProofWithData {
    pub fn new(proofs: Vec<ProofOfStore>) -> Self {
        Self {
            proofs,
            status: Arc::new(Mutex::new(None)),
        }
    }

    pub fn extend(&mut self, other: ProofWithData) {
        let other_data_status = other.status.lock().as_mut().unwrap().take();
        self.proofs.extend(other.proofs);
        let mut status = self.status.lock();
        if status.is_none() {
            *status = Some(other_data_status);
        } else {
            status.as_mut().unwrap().extend(other_data_status);
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ProofWithDataWithTxnLimit {
    pub proof_with_data: ProofWithData,
    pub max_txns_to_execute: Option<usize>,
}

impl PartialEq for ProofWithDataWithTxnLimit {
    fn eq(&self, other: &Self) -> bool {
        self.proof_with_data == other.proof_with_data
            && self.max_txns_to_execute == other.max_txns_to_execute
    }
}

impl Eq for ProofWithDataWithTxnLimit {}

impl ProofWithDataWithTxnLimit {
    pub fn new(proof_with_data: ProofWithData, max_txns_to_execute: Option<usize>) -> Self {
        Self {
            proof_with_data,
            max_txns_to_execute,
        }
    }

    pub fn extend(&mut self, other: ProofWithDataWithTxnLimit) {
        self.proof_with_data.extend(other.proof_with_data);
        // InQuorumStoreWithLimit TODO: what is the right logic here ???
        if self.max_txns_to_execute.is_none() {
            self.max_txns_to_execute = other.max_txns_to_execute;
        }
    }
}

/// The payload in block.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub enum Payload {
    DirectMempool(Vec<SignedTransaction>),
    InQuorumStore(ProofWithData),
    InQuorumStoreWithLimit(ProofWithDataWithTxnLimit),
}

impl Payload {
    pub fn transform_to_quorum_store_v2(self, max_txns_to_execute: Option<usize>) -> Self {
        match self {
            Payload::InQuorumStore(proof_with_status) => Payload::InQuorumStoreWithLimit(
                ProofWithDataWithTxnLimit::new(proof_with_status, max_txns_to_execute),
            ),
            Payload::InQuorumStoreWithLimit(_) => {
                panic!("Payload is already in quorumStoreV2 format");
            },
            Payload::DirectMempool(_) => {
                panic!("Payload is in direct mempool format");
            },
        }
    }

    pub fn empty(quorum_store_enabled: bool) -> Self {
        if quorum_store_enabled {
            Payload::InQuorumStore(ProofWithData::new(Vec::new()))
        } else {
            Payload::DirectMempool(Vec::new())
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Payload::DirectMempool(txns) => txns.len(),
            Payload::InQuorumStore(proof_with_status) => proof_with_status
                .proofs
                .iter()
                .map(|proof| proof.num_txns() as usize)
                .sum(),
            Payload::InQuorumStoreWithLimit(proof_with_status) => {
                let num_txns = proof_with_status
                    .proof_with_data
                    .proofs
                    .iter()
                    .map(|proof| proof.num_txns() as usize)
                    .sum();
                if proof_with_status.max_txns_to_execute.is_some() {
                    min(proof_with_status.max_txns_to_execute.unwrap(), num_txns)
                } else {
                    num_txns
                }
            },
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Payload::DirectMempool(txns) => txns.is_empty(),
            Payload::InQuorumStore(proof_with_status) => proof_with_status.proofs.is_empty(),
            Payload::InQuorumStoreWithLimit(proof_with_status) => {
                proof_with_status.proof_with_data.proofs.is_empty()
                    || (proof_with_status.max_txns_to_execute.is_some()
                        && proof_with_status.max_txns_to_execute.unwrap() == 0)
            },
        }
    }

    pub fn extend(&mut self, other: Payload) {
        match (self, other) {
            (Payload::DirectMempool(v1), Payload::DirectMempool(v2)) => v1.extend(v2),
            (Payload::InQuorumStore(p1), Payload::InQuorumStore(p2)) => p1.extend(p2),
            (Payload::InQuorumStoreWithLimit(p1), Payload::InQuorumStoreWithLimit(p2)) => {
                p1.extend(p2)
            },
            (_, _) => unreachable!(),
        }
    }

    pub fn is_direct(&self) -> bool {
        matches!(self, Payload::DirectMempool(_))
    }

    /// This is computationally expensive on the first call
    pub fn size(&self) -> usize {
        match self {
            Payload::DirectMempool(txns) => txns
                .par_iter()
                .with_min_len(100)
                .map(|txn| txn.raw_txn_bytes_len())
                .sum(),
            Payload::InQuorumStore(proof_with_status) => proof_with_status
                .proofs
                .iter()
                .map(|proof| proof.num_bytes() as usize)
                .sum(),
            // We dedeup, shuffle and finally truncate the txns in the payload to the length == 'max_txns_to_execute'.
            // Hence, it makes sense to pass the full size of the payload here.
            Payload::InQuorumStoreWithLimit(proof_with_status) => proof_with_status
                .proof_with_data
                .proofs
                .iter()
                .map(|proof| proof.num_bytes() as usize)
                .sum(),
        }
    }

    pub fn verify(
        &self,
        validator: &ValidatorVerifier,
        quorum_store_enabled: bool,
    ) -> anyhow::Result<()> {
        match (quorum_store_enabled, self) {
            (false, Payload::DirectMempool(_)) => Ok(()),
            (true, Payload::InQuorumStore(proof_with_status)) => {
                proof_with_status
                    .proofs
                    .par_iter()
                    .with_min_len(4)
                    .try_for_each(|proof| proof.verify(validator))?;
                Ok(())
            },
            (true, Payload::InQuorumStoreWithLimit(proof_with_status)) => {
                proof_with_status
                    .proof_with_data
                    .proofs
                    .par_iter()
                    .with_min_len(4)
                    .try_for_each(|proof| proof.verify(validator))?;
                Ok(())
            },
            (_, _) => Err(anyhow::anyhow!(
                "Wrong payload type. Expected Payload::InQuorumStore {} got {} ",
                quorum_store_enabled,
                self
            )),
        }
    }
}

impl fmt::Display for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Payload::DirectMempool(txns) => {
                write!(f, "InMemory txns: {}", txns.len())
            },
            Payload::InQuorumStore(proof_with_status) => {
                write!(f, "InMemory proofs: {}", proof_with_status.proofs.len())
            },
            Payload::InQuorumStoreWithLimit(proof_with_status) => {
                write!(
                    f,
                    "InMemory proofs: {}",
                    proof_with_status.proof_with_data.proofs.len()
                )
            },
        }
    }
}

/// The payload to filter.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub enum PayloadFilter {
    DirectMempool(Vec<TransactionSummary>),
    InQuorumStore(HashSet<BatchInfo>),
    Empty,
}

impl From<&Vec<&Payload>> for PayloadFilter {
    fn from(exclude_payloads: &Vec<&Payload>) -> Self {
        if exclude_payloads.is_empty() {
            return PayloadFilter::Empty;
        }
        let direct_mode = exclude_payloads.iter().any(|payload| payload.is_direct());

        if direct_mode {
            let mut exclude_txns = Vec::new();
            for payload in exclude_payloads {
                if let Payload::DirectMempool(txns) = payload {
                    for txn in txns {
                        exclude_txns.push(TransactionSummary {
                            sender: txn.sender(),
                            sequence_number: txn.sequence_number(),
                        });
                    }
                }
            }
            PayloadFilter::DirectMempool(exclude_txns)
        } else {
            let mut exclude_proofs = HashSet::new();
            for payload in exclude_payloads {
                match payload {
                    Payload::InQuorumStore(proof_with_status) => {
                        for proof in &proof_with_status.proofs {
                            exclude_proofs.insert(proof.info().clone());
                        }
                    },
                    Payload::InQuorumStoreWithLimit(proof_with_status) => {
                        for proof in &proof_with_status.proof_with_data.proofs {
                            exclude_proofs.insert(proof.info().clone());
                        }
                    },
                    Payload::DirectMempool(_) => {
                        error!("DirectMempool payload in InQuorumStore filter");
                    },
                }
            }
            PayloadFilter::InQuorumStore(exclude_proofs)
        }
    }
}

impl fmt::Display for PayloadFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PayloadFilter::DirectMempool(excluded_txns) => {
                let mut txns_str = "".to_string();
                for tx in excluded_txns.iter() {
                    write!(txns_str, "{} ", tx)?;
                }
                write!(f, "{}", txns_str)
            },
            PayloadFilter::InQuorumStore(excluded_proofs) => {
                let mut proofs_str = "".to_string();
                for proof in excluded_proofs.iter() {
                    write!(proofs_str, "{} ", proof.digest())?;
                }
                write!(f, "{}", proofs_str)
            },
            PayloadFilter::Empty => {
                write!(f, "Empty filter")
            },
        }
    }
}
