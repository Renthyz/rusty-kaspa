use crate::core::model::{CompactUtxoCollection, CompactUtxoEntry, UtxoSetByScriptPublicKey};

use consensus_core::tx::{
    ScriptPublicKey, ScriptPublicKeyVersion, ScriptPublicKeys, ScriptVec, TransactionIndexType, TransactionOutpoint,
};
use database::prelude::{CachedDbAccess, DirectDbWriter, StoreResult, DB};
use hashes::Hash;
use std::mem::size_of;
use std::sync::Arc;

// Prefixes:

/// Prefixes the [ScriptPublicKey] indexed utxo set.
pub const UTXO_SET_PREFIX: &[u8] = b"utxo-set";

pub const VERSION_TYPE_SIZE: usize = size_of::<ScriptPublicKeyVersion>(); // Const since we need to re-use this a few times.

/// [`ScriptPublicKeyBucket`].
/// Consists of 2 bytes of little endian [VersionType] bytes, followed by a variable size of [ScriptVec].
#[derive(Eq, Hash, PartialEq, Debug, Clone)]
struct ScriptPublicKeyBucket(Vec<u8>);

impl From<&ScriptPublicKey> for ScriptPublicKeyBucket {
    fn from(script_public_key: &ScriptPublicKey) -> Self {
        let mut bytes: Vec<u8> = Vec::with_capacity(VERSION_TYPE_SIZE + script_public_key.script().len());
        bytes.extend_from_slice(&script_public_key.version().to_le_bytes());
        bytes.extend_from_slice(script_public_key.script());
        Self(bytes)
    }
}

impl From<ScriptPublicKeyBucket> for ScriptPublicKey {
    fn from(bucket: ScriptPublicKeyBucket) -> Self {
        let version = ScriptPublicKeyVersion::from_le_bytes(
            <[u8; VERSION_TYPE_SIZE]>::try_from(&bucket.0[..VERSION_TYPE_SIZE]).expect("expected version size"),
        );

        let script = ScriptVec::from_slice(&bucket.0[VERSION_TYPE_SIZE..]);

        Self::new(version, script)
    }
}

impl AsRef<[u8]> for ScriptPublicKeyBucket {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

// Keys:

// TransactionOutpoint:
/// Size of the [TransactionOutpointKey] in bytes.
pub const TRANSACTION_OUTPOINT_KEY_SIZE: usize = hashes::HASH_SIZE + size_of::<TransactionIndexType>();

/// [TransactionOutpoint] key which references the [CompactUtxoEntry] within a [ScriptPublicKeyBucket]
/// Consists of 32 bytes of [TransactionId], followed by 4 bytes of little endian [TransactionIndexType]
#[derive(Eq, Hash, PartialEq, Debug, Copy, Clone)]
struct TransactionOutpointKey([u8; TRANSACTION_OUTPOINT_KEY_SIZE]);

impl From<TransactionOutpointKey> for TransactionOutpoint {
    fn from(key: TransactionOutpointKey) -> Self {
        let transaction_id = Hash::from_slice(&key.0[..hashes::HASH_SIZE]);
        let index = TransactionIndexType::from_le_bytes(
            <[u8; std::mem::size_of::<TransactionIndexType>()]>::try_from(&key.0[hashes::HASH_SIZE..]).expect("expected index size"),
        );
        Self::new(transaction_id, index)
    }
}

impl From<&TransactionOutpoint> for TransactionOutpointKey {
    fn from(outpoint: &TransactionOutpoint) -> Self {
        let mut bytes = [0; TRANSACTION_OUTPOINT_KEY_SIZE];
        bytes[..hashes::HASH_SIZE].copy_from_slice(&outpoint.transaction_id.as_bytes());
        bytes[hashes::HASH_SIZE..].copy_from_slice(&outpoint.index.to_le_bytes());
        Self(bytes)
    }
}

impl AsRef<[u8]> for TransactionOutpointKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Full [CompactUtxoEntry] access key.
/// Consists of variable amount of bytes of [ScriptPublicKeyBucket], and 36 bytes of [TransactionOutpointKey]
#[derive(Eq, Hash, PartialEq, Debug, Clone)]
struct UtxoEntryFullAccessKey(Arc<Vec<u8>>);

impl UtxoEntryFullAccessKey {
    /// Creates a new [UtxoEntryFullAccessKey] from a [ScriptPublicKeyBucket] and [TransactionOutpointKey].
    pub fn new(script_public_key_bucket: ScriptPublicKeyBucket, transaction_outpoint_key: TransactionOutpointKey) -> Self {
        let mut bytes = Vec::with_capacity(TRANSACTION_OUTPOINT_KEY_SIZE + script_public_key_bucket.as_ref().len());
        bytes.extend_from_slice(script_public_key_bucket.as_ref());
        bytes.extend_from_slice(transaction_outpoint_key.as_ref());
        Self(Arc::new(bytes))
    }
}

impl AsRef<[u8]> for UtxoEntryFullAccessKey {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

// Traits:

pub trait UtxoSetByScriptPublicKeyStoreReader {
    /// Get [UtxoSetByScriptPublicKey] set by queried [ScriptPublicKeys],
    fn get_utxos_from_script_public_keys(&self, script_public_keys: &ScriptPublicKeys) -> StoreResult<UtxoSetByScriptPublicKey>;
}

pub trait UtxoSetByScriptPublicKeyStore: UtxoSetByScriptPublicKeyStoreReader {
    /// remove [UtxoSetByScriptPublicKey] from the [UtxoSetByScriptPublicKeyStore].
    fn remove_utxo_entries(&mut self, utxo_entries: &UtxoSetByScriptPublicKey) -> StoreResult<()>;

    /// add [UtxoSetByScriptPublicKey] into the [UtxoSetByScriptPublicKeyStore].
    fn add_utxo_entries(&mut self, utxo_entries: &UtxoSetByScriptPublicKey) -> StoreResult<()>;

    /// removes all entries in the cache and db, besides prefixes themselves.
    fn delete_all(&mut self) -> StoreResult<()>;
}

// Implementations:

#[derive(Clone)]
pub struct DbUtxoSetByScriptPublicKeyStore {
    db: Arc<DB>,
    access: CachedDbAccess<UtxoEntryFullAccessKey, CompactUtxoEntry>,
}

impl DbUtxoSetByScriptPublicKeyStore {
    pub fn new(db: Arc<DB>, cache_size: u64) -> Self {
        Self { db: Arc::clone(&db), access: CachedDbAccess::new(db, cache_size, UTXO_SET_PREFIX.to_vec()) }
    }
}

impl UtxoSetByScriptPublicKeyStoreReader for DbUtxoSetByScriptPublicKeyStore {
    // compared to go-kaspad this gets transaction outpoints from multiple script public keys at once.
    // TODO: probably ideal way to retrieve is to return a chained iterator which can be used to chunk results and propagate utxo entries
    // to the rpc via pagination, this would alleviate the memory footprint of script public keys with large amount of utxos.
    fn get_utxos_from_script_public_keys(&self, script_public_keys: &ScriptPublicKeys) -> StoreResult<UtxoSetByScriptPublicKey> {
        let mut utxos_by_script_public_keys = UtxoSetByScriptPublicKey::new();
        for script_public_key in script_public_keys.iter() {
            let script_public_key_bucket = ScriptPublicKeyBucket::from(script_public_key);
            let utxos_by_script_public_keys_inner = CompactUtxoCollection::from_iter(
                self.access
                    .seek_iterator::<TransactionOutpoint, CompactUtxoEntry>(
                        Some(script_public_key_bucket.as_ref()),
                        None,
                        usize::MAX,
                        false,
                    )
                    .map(move |value| {
                        let (k, v) = value.expect("expected `key: TransactionOutpoint`, `value: CompactUtxoEntry`");
                        (k, v)
                    }),
            );
            utxos_by_script_public_keys.insert(ScriptPublicKey::from(script_public_key_bucket), utxos_by_script_public_keys_inner);
        }
        Ok(utxos_by_script_public_keys)
    }
}

impl UtxoSetByScriptPublicKeyStore for DbUtxoSetByScriptPublicKeyStore {
    fn remove_utxo_entries(&mut self, utxo_entries: &UtxoSetByScriptPublicKey) -> StoreResult<()> {
        if utxo_entries.is_empty() {
            return Ok(());
        }

        let mut writer = DirectDbWriter::new(&self.db);

        let mut to_remove = utxo_entries.iter().flat_map(move |(script_public_key, compact_utxo_collection)| {
            compact_utxo_collection.iter().map(move |(transaction_outpoint, _)| {
                UtxoEntryFullAccessKey::new(
                    ScriptPublicKeyBucket::from(script_public_key),
                    TransactionOutpointKey::from(transaction_outpoint),
                )
            })
        });

        self.access.delete_many(&mut writer, &mut to_remove)?;

        Ok(())
    }

    fn add_utxo_entries(&mut self, utxo_entries: &UtxoSetByScriptPublicKey) -> StoreResult<()> {
        if utxo_entries.is_empty() {
            return Ok(());
        }

        let mut writer = DirectDbWriter::new(&self.db);

        let mut to_add = utxo_entries.iter().flat_map(move |(script_public_key, compact_utxo_collection)| {
            compact_utxo_collection.iter().map(move |(transaction_outpoint, compact_utxo)| {
                (
                    UtxoEntryFullAccessKey::new(
                        ScriptPublicKeyBucket::from(script_public_key),
                        TransactionOutpointKey::from(transaction_outpoint),
                    ),
                    *compact_utxo,
                )
            })
        });

        self.access.write_many(&mut writer, &mut to_add)?;

        Ok(())
    }

    /// Removes all entries in the cache and db, besides prefixes themselves.
    fn delete_all(&mut self) -> StoreResult<()> {
        let mut writer = DirectDbWriter::new(&self.db);
        self.access.delete_all(&mut writer)
    }
}