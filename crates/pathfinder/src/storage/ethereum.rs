use rusqlite::{named_params, Transaction};

use crate::core::{
    EthereumBlockHash, EthereumBlockNumber, EthereumTransactionHash, EthereumTransactionIndex,
};

/// Stores basic information about an Ethereum block, enough to descibe it as a unique point
/// of origin. This lets us link StarkNet information to a point in Ethereum's history.
///
/// Specifically, this stores an Ethereum block's
/// - [block hash](EthereumBlockHash)
/// - [block number](EthereumBlockNumber)
pub struct EthereumBlocksTable {}

impl EthereumBlocksTable {
    /// Inserts a new Ethereum block with the given [hash](EthereumBlockHash) and [number](EthereumBlockNumber).
    ///
    /// Does nothing if the hash already exists.
    pub fn insert(
        transaction: &Transaction,
        hash: EthereumBlockHash,
        number: EthereumBlockNumber,
    ) -> anyhow::Result<()> {
        transaction.execute(
            r"INSERT INTO ethereum_blocks ( hash,  number)
                                       VALUES (:hash, :number)
                                       ON CONFLICT DO NOTHING",
            named_params! {
                ":hash": hash.0.as_bytes(),
                ":number": number.0,
            },
        )?;
        Ok(())
    }
}

/// Stores basic information about an Ethereum transaction, enough to descibe it as a unique point
/// of origin. This lets us link StarkNet information to a point in Ethereum's history.
///
/// Specifically, this stores an Ethereum transactions
/// - [transaction hash](EthereumTransactionHash)
/// - [transaction index](EthereumTransactionIndex)
/// - [block hash](EthereumBlockHash)
pub struct EthereumTransactionsTable {}

impl EthereumTransactionsTable {
    /// Insert a new Ethereum transaction.
    ///
    /// Does nothing if the ethereum hash already exists.
    ///
    /// Note that [block_hash](EthereumBlockHash) must reference an
    /// Ethereum block stored in [EthereumBlocksTable].
    pub fn insert(
        transaction: &Transaction,
        block_hash: EthereumBlockHash,
        tx_hash: EthereumTransactionHash,
        tx_index: EthereumTransactionIndex,
    ) -> anyhow::Result<()> {
        transaction.execute(
            r"INSERT INTO ethereum_transactions ( hash,  idx,  block_hash)
                                             VALUES (:hash, :idx, :block_hash)
                                             ON CONFLICT DO NOTHING",
            named_params! {
                ":hash": tx_hash.0.as_bytes(),
                ":idx": tx_index.0,
                ":block_hash": block_hash.0.as_bytes(),
            },
        )?;
        Ok(())
    }
}
