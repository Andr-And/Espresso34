#![deny(warnings)]

use zerok_macros::*;

pub use crate::full_persistence::FullPersistence;
pub use crate::lw_persistence::LWPersistence;
pub use crate::set_merkle_tree::*;
pub use crate::util::canonical;
use arbitrary::{Arbitrary, Unstructured};
use ark_serialize::*;
use canonical::deserialize_canonical_bytes;
use canonical::CanonicalBytes;
use commit::{Commitment, Committable};
use core::fmt::Debug;
use jf_aap::{
    errors::TxnApiError,
    proof::{freeze::FreezeProvingKey, mint::MintProvingKey, transfer::TransferProvingKey},
    structs::{Nullifier, RecordCommitment},
    txn_batch_verify, MerkleCommitment, MerkleFrontier, MerkleLeafProof, MerkleTree, NodeValue,
    TransactionNote, TransactionVerifyingKey,
};
use jf_utils::tagged_blob;
use phaselock::{traits::state::State, BlockContents, H_256};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use snafu::Snafu;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::iter::FromIterator;
use std::ops::Bound::*;

pub const MERKLE_HEIGHT: u8 = 20 /*H*/;

// TODO
pub struct LedgerRecordCommitment(pub RecordCommitment);

// TODO
#[derive(Debug, Clone, CanonicalSerialize, CanonicalDeserialize, PartialEq, Eq, Hash)]
pub struct Transaction(pub TransactionNote);

#[derive(
    Debug,
    Clone,
    CanonicalSerialize,
    CanonicalDeserialize,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
)]
pub struct ElaboratedTransaction {
    pub txn: TransactionNote,
    pub proofs: Vec<SetMerkleProof>,
}

impl ElaboratedTransaction {
    pub(crate) fn etxn_hash(&self) -> ElaboratedTransactionHash {
        ElaboratedTransactionHash(self.commit())
    }
}

#[ser_test(arbitrary)]
#[tagged_blob("TXN")]
#[derive(
    Arbitrary, Clone, Debug, PartialEq, Eq, Hash, CanonicalSerialize, CanonicalDeserialize,
)]
pub struct ElaboratedTransactionHash(pub(crate) Commitment<ElaboratedTransaction>);

#[ser_test]
#[derive(
    Default,
    Debug,
    Clone,
    CanonicalSerialize,
    CanonicalDeserialize,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Hash,
)]
pub struct Block(pub Vec<TransactionNote>);

// A block with nullifier set non-membership proofs
#[ser_test]
#[derive(
    Default,
    Debug,
    Clone,
    CanonicalSerialize,
    CanonicalDeserialize,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
)]
pub struct ElaboratedBlock {
    pub block: Block,
    pub proofs: Vec<Vec<SetMerkleProof>>,
}

impl Committable for ElaboratedBlock {
    fn commit(&self) -> Commitment<Self> {
        commit::RawCommitmentBuilder::new("ElaboratedBlock")
            .field("Block contents", self.block.commit())
            .var_size_field("Block proofs", &canonical::serialize(&self.proofs).unwrap())
            .finalize()
    }
}

impl Committable for ElaboratedTransaction {
    fn commit(&self) -> Commitment<Self> {
        commit::RawCommitmentBuilder::new("ElaboratedTransaction")
            .field("Txn contents", self.txn.commit())
            .var_size_field("Txn proofs", &canonical::serialize(&self.proofs).unwrap())
            .finalize()
    }
}

impl BlockContents<H_256> for ElaboratedBlock {
    type Transaction = ElaboratedTransaction;
    type Error = ValidationError;

    fn add_transaction_raw(&self, txn: &ElaboratedTransaction) -> Result<Self, ValidationError> {
        let mut ret = self.clone();

        let mut nulls = self
            .block
            .0
            .iter()
            .flat_map(|x| x.nullifiers().into_iter())
            .collect::<HashSet<_>>();
        for n in txn.txn.nullifiers().iter() {
            if nulls.contains(n) {
                return Err(ValidationError::ConflictingNullifiers {});
            }
            nulls.insert(*n);
        }

        ret.block.0.push(txn.txn.clone());
        ret.proofs.push(txn.proofs.clone());

        Ok(ret)
    }

    fn hash(&self) -> phaselock::BlockHash<H_256> {
        use std::convert::TryInto;

        phaselock::BlockHash::<H_256>::from_array(self.commit().try_into().unwrap())
    }

    fn hash_bytes(bytes: &[u8]) -> phaselock::BlockHash<H_256> {
        use std::convert::TryInto;
        // TODO: fix this hack, it is specifically working around the
        // misuse-preventing `T: Committable` on `RawCommitmentBuilder`
        let ret = commit::RawCommitmentBuilder::<Block>::new("PhaseLock bytes")
            .var_size_bytes(bytes)
            .finalize();
        phaselock::BlockHash::<H_256>::from_array(ret.try_into().unwrap())
    }

    fn hash_transaction(txn: &ElaboratedTransaction) -> phaselock::BlockHash<H_256> {
        use std::convert::TryInto;

        phaselock::BlockHash::<H_256>::from_array(txn.commit().try_into().unwrap())
    }
}

pub mod key_set {
    use super::*;

    #[derive(Debug, Snafu)]
    #[snafu(visibility = "pub")]
    pub enum Error {
        DuplicateKeys {
            num_inputs: usize,
            num_outputs: usize,
        },
        NoKeys,
    }

    pub trait SizedKey: CanonicalSerialize + CanonicalDeserialize {
        fn num_inputs(&self) -> usize;
        fn num_outputs(&self) -> usize;
    }

    impl<'a> SizedKey for TransferProvingKey<'a> {
        fn num_inputs(&self) -> usize {
            self.num_input()
        }

        fn num_outputs(&self) -> usize {
            self.num_output()
        }
    }

    impl<'a> SizedKey for FreezeProvingKey<'a> {
        fn num_inputs(&self) -> usize {
            self.num_input()
        }

        fn num_outputs(&self) -> usize {
            self.num_output()
        }
    }

    impl SizedKey for TransactionVerifyingKey {
        fn num_inputs(&self) -> usize {
            match self {
                TransactionVerifyingKey::Transfer(xfr) => xfr.num_input(),
                TransactionVerifyingKey::Freeze(freeze) => freeze.num_input(),
                TransactionVerifyingKey::Mint(_) => 1,
            }
        }

        fn num_outputs(&self) -> usize {
            match self {
                TransactionVerifyingKey::Transfer(xfr) => xfr.num_output(),
                TransactionVerifyingKey::Freeze(freeze) => freeze.num_output(),
                TransactionVerifyingKey::Mint(_) => 2,
            }
        }
    }

    pub trait KeyOrder {
        type SortKey: Ord
            + Debug
            + Clone
            + Serialize
            + for<'a> Deserialize<'a>
            + CanonicalSerialize
            + CanonicalDeserialize;
        fn sort_key(num_inputs: usize, num_outputs: usize) -> Self::SortKey;
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct OrderByInputs;
    impl KeyOrder for OrderByInputs {
        type SortKey = (usize, usize);
        fn sort_key(num_inputs: usize, num_outputs: usize) -> Self::SortKey {
            (num_inputs, num_outputs)
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct OrderByOutputs;
    impl KeyOrder for OrderByOutputs {
        type SortKey = (usize, usize);
        fn sort_key(num_inputs: usize, num_outputs: usize) -> Self::SortKey {
            (num_outputs, num_inputs)
        }
    }

    #[serde_as]
    #[derive(
        Debug,
        Default,
        Clone,
        Serialize,
        Deserialize,
        CanonicalSerialize,
        CanonicalDeserialize,
        PartialEq,
    )]
    #[serde(bound = "K: Serialize + for<'a> Deserialize<'a>")]
    pub struct KeySet<K: SizedKey, Order: KeyOrder = OrderByInputs> {
        // serde_json does not support maps where the keys are not Strings (or easily convertible
        // to/from Strings) so we serialize this map as a sequence of key-value pairs.
        #[serde_as(as = "Vec<(_, _)>")]
        keys: BTreeMap<Order::SortKey, K>,
    }

    impl<K: SizedKey, Order: KeyOrder> KeySet<K, Order> {
        /// Create a new KeySet with the keys in an iterator. `keys` must contain at least one key,
        /// and it must not contain two keys with the same size.
        pub fn new(keys: impl Iterator<Item = K>) -> Result<Self, Error> {
            let mut map = BTreeMap::new();
            for key in keys {
                let sort_key = Order::sort_key(key.num_inputs(), key.num_outputs());
                if map.contains_key(&sort_key) {
                    return Err(Error::DuplicateKeys {
                        num_inputs: key.num_inputs(),
                        num_outputs: key.num_outputs(),
                    });
                }
                map.insert(sort_key, key);
            }
            if map.is_empty() {
                return Err(Error::NoKeys);
            }
            Ok(Self { keys: map })
        }

        /// Get the largest size supported by this KeySet.
        ///
        /// Panics if there are no keys in the KeySet. Since new() requires at least one key, this
        /// can only happen if the KeySet is corrupt (for example, it was deserialized from a
        /// corrupted file).
        pub fn max_size(&self) -> (usize, usize) {
            let key = &self.keys.iter().next_back().unwrap().1;
            (key.num_inputs(), key.num_outputs())
        }

        pub fn key_for_size(&self, num_inputs: usize, num_outputs: usize) -> Option<&K> {
            self.keys.get(&Order::sort_key(num_inputs, num_outputs))
        }

        /// Return the smallest key whose size is at least (num_inputs, num_outputs). If no such key
        /// is available, the error contains the largest size that could have been supported.
        pub fn best_fit_key(
            &self,
            num_inputs: usize,
            num_outputs: usize,
        ) -> Result<(usize, usize, &K), (usize, usize)> {
            self.keys
                .range((
                    Included(Order::sort_key(num_inputs, num_outputs)),
                    Unbounded,
                ))
                .next()
                .map(|(_, key)| (key.num_inputs(), key.num_outputs(), key))
                .ok_or_else(|| self.max_size())
        }

        /// Return the key whose size is (num_inputs, num_outputs).
        pub fn exact_fit_key(&self, num_inputs: usize, num_outputs: usize) -> Option<&K> {
            self.keys.get(&Order::sort_key(num_inputs, num_outputs))
        }

        pub fn iter(&self) -> impl Iterator<Item = &K> {
            self.keys.values()
        }
    }

    impl<K: SizedKey, Order: KeyOrder> FromIterator<K> for KeySet<K, Order> {
        fn from_iter<T: IntoIterator<Item = K>>(iter: T) -> Self {
            Self::new(iter.into_iter()).unwrap()
        }
    }
}
use key_set::KeySet;

#[derive(
    Debug, Clone, CanonicalSerialize, CanonicalDeserialize, Serialize, Deserialize, PartialEq,
)]
pub struct ProverKeySet<'a, Order: key_set::KeyOrder = key_set::OrderByInputs> {
    pub mint: MintProvingKey<'a>,
    pub xfr: KeySet<TransferProvingKey<'a>, Order>,
    pub freeze: KeySet<FreezeProvingKey<'a>, Order>,
}

#[derive(Debug, Clone, CanonicalSerialize, CanonicalDeserialize, Serialize, Deserialize)]
pub struct VerifierKeySet<Order: key_set::KeyOrder = key_set::OrderByInputs> {
    // TODO: is there a way to keep these types distinct?
    pub mint: TransactionVerifyingKey,
    pub xfr: KeySet<TransactionVerifyingKey, Order>,
    pub freeze: KeySet<TransactionVerifyingKey, Order>,
}

// TODO
#[derive(Debug, Snafu, Serialize, Deserialize)]
#[snafu(visibility = "pub(crate)")]
pub enum ValidationError {
    NullifierAlreadyExists {
        nullifier: Nullifier,
    },
    BadNullifierProof {},
    MissingNullifierProof {},
    ConflictingNullifiers {},
    Failed {},
    BadMerkleLength {},
    BadMerkleLeaf {},
    BadMerkleRoot {},
    BadMerklePath {},
    CryptoError {
        // TxnApiError cannot be serialized, and, since it depends on many foreign error types which
        // are not Serialize, it is infeasible to make it serializable. Instead, if we have to
        // serialize this variant, we will serialize Ok(err) to Err(format(err)), and when we
        // deserialize we will at least preserve the variant CryptoError and a String representation
        // of the underlying error.
        #[serde(with = "ser_display")]
        err: Result<TxnApiError, String>,
    },
    UnsupportedTransferSize {
        num_inputs: usize,
        num_outputs: usize,
    },
    UnsupportedFreezeSize {
        num_inputs: usize,
    },
}

pub(crate) mod ser_display {
    use serde::de::{Deserialize, Deserializer};
    use serde::ser::{Serialize, Serializer};
    use std::fmt::Display;

    pub fn serialize<S: Serializer, T: Display>(
        v: &Result<T, String>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let string = match v {
            Ok(v) => format!("{}", v),
            Err(string) => string.clone(),
        };
        Serialize::serialize(&string, s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T>(d: D) -> Result<Result<T, String>, D::Error> {
        Ok(Err(Deserialize::deserialize(d)?))
    }
}

pub(crate) mod ser_debug {
    use serde::de::{Deserialize, Deserializer};
    use serde::ser::{Serialize, Serializer};
    use std::fmt::Debug;

    pub fn serialize<S: Serializer, T: Debug>(
        v: &Result<T, String>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let string = match v {
            Ok(v) => format!("{:?}", v),
            Err(string) => string.clone(),
        };
        Serialize::serialize(&string, s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, T>(d: D) -> Result<Result<T, String>, D::Error> {
        Ok(Err(Deserialize::deserialize(d)?))
    }
}

// TxnApiError doesn't implement Clone :/
impl Clone for ValidationError {
    fn clone(&self) -> Self {
        use ValidationError::*;
        match self {
            NullifierAlreadyExists { nullifier } => NullifierAlreadyExists {
                nullifier: *nullifier,
            },
            BadNullifierProof {} => BadNullifierProof {},
            MissingNullifierProof {} => MissingNullifierProof {},
            ConflictingNullifiers {} => ConflictingNullifiers {},
            Failed {} => Failed {},
            BadMerkleLength {} => BadMerkleLength {},
            BadMerkleLeaf {} => BadMerkleLeaf {},
            BadMerkleRoot {} => BadMerkleRoot {},
            BadMerklePath {} => BadMerklePath {},
            CryptoError { .. } => Failed {},
            UnsupportedTransferSize {
                num_inputs,
                num_outputs,
            } => UnsupportedTransferSize {
                num_inputs: *num_inputs,
                num_outputs: *num_outputs,
            },
            UnsupportedFreezeSize { num_inputs } => UnsupportedFreezeSize {
                num_inputs: *num_inputs,
            },
        }
    }
}

impl Committable for VerifierKeySet {
    fn commit(&self) -> Commitment<Self> {
        commit::RawCommitmentBuilder::new("VerifCRS Comm")
            .var_size_bytes(&canonical::serialize(self).unwrap())
            .finalize()
    }
}

#[ser_test(arbitrary)]
#[tagged_blob("BLOCK")]
#[derive(
    Arbitrary, Debug, Clone, Copy, PartialEq, Eq, Hash, CanonicalSerialize, CanonicalDeserialize,
)]
pub struct BlockCommitment(pub commit::Commitment<Block>);

deserialize_canonical_bytes!(BlockCommitment);

impl Committable for Block {
    fn commit(&self) -> commit::Commitment<Self> {
        commit::RawCommitmentBuilder::new("Block Comm")
            .array_field(
                "txns",
                &self.0.iter().map(|x| x.commit()).collect::<Vec<_>>(),
            )
            .finalize()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordMerkleHistory(pub VecDeque<NodeValue>);

impl Committable for RecordMerkleHistory {
    fn commit(&self) -> commit::Commitment<Self> {
        let mut ret = commit::RawCommitmentBuilder::new("Hist Comm")
            .constant_str("roots")
            .u64(self.0.len() as u64);
        for n in self.0.iter() {
            ret = ret.var_size_bytes(&canonical::serialize(n).unwrap())
        }
        ret.finalize()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordMerkleCommitment(pub MerkleCommitment);

impl Committable for RecordMerkleCommitment {
    fn commit(&self) -> commit::Commitment<Self> {
        commit::RawCommitmentBuilder::new("RMT Comm")
            .constant_str("height")
            .u64(self.0.height as u64)
            .constant_str("num_leaves")
            .u64(self.0.num_leaves)
            .constant_str("root_value")
            .var_size_bytes(&canonical::serialize(&self.0.root_value).unwrap())
            .finalize()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordMerkleFrontier(pub MerkleFrontier);

impl Committable for RecordMerkleFrontier {
    fn commit(&self) -> commit::Commitment<Self> {
        let mut ret = commit::RawCommitmentBuilder::new("RMFrontier");
        match &self.0 {
            MerkleFrontier::Empty { height } => {
                ret = ret.constant_str("empty height").u64(*height as u64);
            }
            MerkleFrontier::Proof(MerkleLeafProof { leaf, path }) => {
                ret = ret
                    .constant_str("leaf")
                    .var_size_bytes(&canonical::serialize(&leaf.0).unwrap())
                    .constant_str("path");
                for step in path.nodes.iter() {
                    ret = ret.var_size_bytes(&canonical::serialize(step).unwrap())
                }
            }
        }
        ret.finalize()
    }
}

pub mod state_comm {
    use super::*;
    use jf_utils::tagged_blob;

    #[ser_test(arbitrary)]
    #[tagged_blob("STATE")]
    #[derive(
        Arbitrary, Debug, Clone, Copy, CanonicalSerialize, CanonicalDeserialize, PartialEq, Eq, Hash,
    )]
    pub struct LedgerStateCommitment(pub Commitment<LedgerCommitmentOpening>);

    impl From<Commitment<LedgerCommitmentOpening>> for LedgerStateCommitment {
        fn from(x: Commitment<LedgerCommitmentOpening>) -> Self {
            Self(x)
        }
    }

    impl From<LedgerStateCommitment> for Commitment<LedgerCommitmentOpening> {
        fn from(x: LedgerStateCommitment) -> Self {
            x.0
        }
    }

    impl AsRef<[u8]> for LedgerStateCommitment {
        fn as_ref(&self) -> &[u8] {
            self.0.as_ref()
        }
    }

    #[derive(Debug)]
    pub struct LedgerCommitmentOpening {
        pub prev_commit_time: u64,
        pub prev_state: Option<state_comm::LedgerStateCommitment>,
        pub verif_crs: Commitment<VerifierKeySet>,
        pub record_merkle_commitment: Commitment<RecordMerkleCommitment>,
        pub record_merkle_frontier: Commitment<RecordMerkleFrontier>,
        pub past_record_merkle_roots: Commitment<RecordMerkleHistory>,
        pub nullifiers: set_hash::Hash,
        pub prev_block: Commitment<Block>,
    }

    impl Committable for LedgerCommitmentOpening {
        fn commit(&self) -> Commitment<Self> {
            commit::RawCommitmentBuilder::new("Ledger Comm")
                .u64_field("prev_commit_time", self.prev_commit_time)
                .array_field(
                    "prev_state",
                    &self
                        .prev_state
                        .iter()
                        .cloned()
                        .map(Commitment::<Self>::from)
                        .collect::<Vec<_>>(),
                )
                .field("verif_crs", self.verif_crs)
                .field("record_merkle_commitment", self.record_merkle_commitment)
                .field("record_merkle_frontier", self.record_merkle_frontier)
                .field("past_record_merkle_roots", self.past_record_merkle_roots)
                .field("nullifiers", self.nullifiers.into())
                .field("prev_block", self.prev_block)
                .finalize()
        }
    }
}

#[ser_test(arbitrary, ark(false))]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatorState {
    pub prev_commit_time: u64,
    pub prev_state: Option<state_comm::LedgerStateCommitment>,
    pub verif_crs: VerifierKeySet,
    // The current record Merkle commitment
    pub record_merkle_commitment: MerkleCommitment,
    // The current frontier of the record Merkle tree
    pub record_merkle_frontier: MerkleFrontier,
    // A list of recent record Merkle root hashes for validating slightly-out- of date transactions.
    pub past_record_merkle_roots: RecordMerkleHistory,
    pub nullifiers_root: set_hash::Hash,
    pub prev_block: BlockCommitment,
}

impl ValidatorState {
    // How many previous record Merkle tree root hashes the validator should remember.
    //
    // Transactions can be validated without resubmitting or regenerating the ZKPs as long as they
    // were generated using a validator state that is at most RECORD_ROOT_HISTORY_SIZE states before
    // the current one.
    pub const RECORD_ROOT_HISTORY_SIZE: usize = 10;

    pub fn new(verif_crs: VerifierKeySet, record_merkle_frontier: MerkleTree) -> Self {
        let nullifiers: SetMerkleTree = Default::default();

        Self {
            prev_commit_time: 0u64,
            prev_state: None,
            verif_crs,
            record_merkle_commitment: record_merkle_frontier.commitment(),
            record_merkle_frontier: record_merkle_frontier.frontier(),
            past_record_merkle_roots: RecordMerkleHistory(VecDeque::with_capacity(
                Self::RECORD_ROOT_HISTORY_SIZE,
            )),
            nullifiers_root: nullifiers.hash(),
            prev_block: BlockCommitment(Block::default().commit()),
        }
    }

    pub fn commit(&self) -> state_comm::LedgerStateCommitment {
        let inputs = state_comm::LedgerCommitmentOpening {
            prev_commit_time: self.prev_commit_time,
            prev_state: self.prev_state,
            verif_crs: self.verif_crs.commit(),
            record_merkle_commitment: RecordMerkleCommitment(self.record_merkle_commitment)
                .commit(),
            record_merkle_frontier: RecordMerkleFrontier(self.record_merkle_frontier.clone())
                .commit(),
            // We need to include all the cached past record Merkle roots in the state commitment,
            // even though they are not part of the current ledger state, because they affect
            // validation: two validators with different caches will be able to validate different
            // blocks.
            //
            // Note that this requires correct validators to agree on the number of cached past root
            // hashes, since all the cached hashes are included in the state commitment and are thus
            // part of the observable state of the ledger. This prevents heavyweight validators from
            // caching extra past roots and thereby making it easier to verify transactions, but
            // because root hashes are small, it should be possible to find a value of
            // RECORD_ROOT_HISTORY_SIZE which strikes a balance between small space requirements (so
            // that lightweight validators can keep up with the cache) and covering enough of
            // history to make it easy for clients. If this is not possible, lightweight validators
            // could also store a sparse history, and when they encounter a root hash that they do
            // not have cached, they could ask a full validator for a proof that that hash was once
            // the root of the record Merkle tree.
            past_record_merkle_roots: self.past_record_merkle_roots.commit(),

            nullifiers: self.nullifiers_root,
            prev_block: self.prev_block.0,
        };
        // dbg!(&inputs);
        inputs.commit().into()
    }

    pub fn validate_block_check(
        &self,
        now: u64,
        txns: Block,
        null_pfs: Vec<Vec<SetMerkleProof>>,
    ) -> Result<(Block, Vec<Vec<SetMerkleProof>>), ValidationError> {
        let mut nulls = HashSet::new();
        use ValidationError::*;
        for (pf, n) in null_pfs
            .iter()
            .zip(txns.0.iter())
            .flat_map(|(pfs, txn)| pfs.iter().zip(txn.nullifiers().into_iter()))
        {
            if nulls.contains(&n)
                || pf
                    .check(n, &self.nullifiers_root)
                    .map_err(|_| BadNullifierProof {})?
            {
                return Err(NullifierAlreadyExists { nullifier: n });
            }

            nulls.insert(n);
        }

        let verif_keys = txns
            .0
            .iter()
            .map(|txn| match txn {
                TransactionNote::Mint(_) => Ok(&self.verif_crs.mint),
                TransactionNote::Transfer(note) => {
                    let num_inputs = note.inputs_nullifiers.len();
                    let num_outputs = note.output_commitments.len();
                    self.verif_crs
                        .xfr
                        .key_for_size(num_inputs, num_outputs)
                        .ok_or(UnsupportedTransferSize {
                            num_inputs,
                            num_outputs,
                        })
                }
                TransactionNote::Freeze(note) => {
                    let num_inputs = note.input_nullifiers.len();
                    let num_outputs = note.output_commitments.len();
                    self.verif_crs
                        .freeze
                        .key_for_size(num_inputs, num_outputs)
                        .ok_or(UnsupportedFreezeSize { num_inputs })
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        if !txns.0.is_empty() {
            txn_batch_verify(
                &txns.0,
                &txns
                    .0
                    .iter()
                    .map(|note| {
                        // Only validate transactions if we can confirm that the record Merkle root
                        // they were generated with is a valid previous or current ledger state.
                        if self.record_merkle_commitment.root_value == note.merkle_root()
                            || self
                                .past_record_merkle_roots
                                .0
                                .contains(&note.merkle_root())
                        {
                            Ok(note.merkle_root())
                        } else {
                            Err(BadMerkleRoot {})
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                now,
                &verif_keys,
            )
            .map_err(|err| CryptoError { err: Ok(err) })?;
        }

        Ok((txns, null_pfs))
    }

    /// Performs validation for a block, updating the ValidatorState.
    ///
    /// For a given instance of ValidatorState, all calls to validate_and_apply must pass the
    /// same remember_commitments value. This identity holds for clones of the original as well.
    pub fn validate_and_apply(
        &mut self,
        now: u64,
        txns: Block,
        null_pfs: Vec<Vec<SetMerkleProof>>,
    ) -> Result<Vec<u64> /* new uids */, ValidationError> {
        // If the block successfully validates, and the nullifier proofs apply correctly,
        // the remaining (mutating) operations cannot fail, as this would result in an
        // inconsistent state. Currenlty, no operations after the first assignement to a member
        // of self have a possible error; this must remain true if code changes.
        let (txns, _null_pfs) = self.validate_block_check(now, txns, null_pfs.clone())?;
        let comm = self.commit();
        self.prev_commit_time = now;
        self.prev_block = BlockCommitment(txns.commit());

        let nullifiers = txns
            .0
            .iter()
            .zip(null_pfs.into_iter())
            .flat_map(|(txn, null_pfs)| txn.nullifiers().into_iter().zip(null_pfs.into_iter()))
            .collect();

        self.nullifiers_root = set_merkle_lw_multi_insert(nullifiers, self.nullifiers_root)
            .map_err(|_| ValidationError::BadNullifierProof {})?
            .0;

        let mut record_merkle_frontier = MerkleTree::restore_from_frontier(
            self.record_merkle_commitment,
            &self.record_merkle_frontier,
        )
        .ok_or(ValidationError::BadMerklePath {})?;
        let mut ret = vec![];
        let mut uid = self.record_merkle_commitment.num_leaves;
        for o in txns
            .0
            .iter()
            .flat_map(|x| x.output_commitments().into_iter())
        {
            record_merkle_frontier.push(o.to_field_element());
            if uid > 0 {
                record_merkle_frontier.forget(uid - 1).expect_ok().unwrap();
            }
            ret.push(uid);
            uid += 1;
            assert_eq!(uid, record_merkle_frontier.num_leaves());
        }

        if self.past_record_merkle_roots.0.len() >= Self::RECORD_ROOT_HISTORY_SIZE {
            self.past_record_merkle_roots.0.pop_back();
        }
        self.past_record_merkle_roots
            .0
            .push_front(self.record_merkle_commitment.root_value);
        self.record_merkle_commitment = record_merkle_frontier.commitment();
        self.record_merkle_frontier = record_merkle_frontier.frontier();
        self.prev_state = Some(comm);
        Ok(ret)
    }
}

impl<'a> Arbitrary<'a> for ValidatorState {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(crate::testing::MultiXfrTestState::initialize(
            u.arbitrary()?,
            u.arbitrary()?,
            u.arbitrary()?,
            (u.arbitrary()?, u.arbitrary()?),
        )
        .unwrap()
        .validator)
    }
}

impl PartialEq for ValidatorState {
    fn eq(&self, other: &ValidatorState) -> bool {
        self.commit() == other.commit()
    }
}

impl Eq for ValidatorState {}

impl Hash for ValidatorState {
    fn hash<H: Hasher>(&self, state: &mut H) {
        <Commitment<_> as Hash>::hash(&self.commit().0, state);
    }
}

impl State<H_256> for ValidatorState {
    type Error = ValidationError;

    type Block = ElaboratedBlock;

    fn next_block(&self) -> Self::Block {
        Self::Block::default()
    }

    fn validate_block(&self, block: &Self::Block) -> bool {
        self.validate_block_check(
            self.prev_commit_time + 1,
            block.block.clone(),
            block.proofs.clone(),
        )
        .is_ok()
    }

    fn append(&self, block: &Self::Block) -> Result<Self, Self::Error> {
        let mut state = self.clone();
        state.validate_and_apply(
            state.prev_commit_time + 1,
            block.block.clone(),
            block.proofs.clone(),
        )?;
        Ok(state)
    }

    fn on_commit(&self) {}
}
