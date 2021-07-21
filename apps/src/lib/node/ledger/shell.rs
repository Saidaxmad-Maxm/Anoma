use std::convert::{TryFrom, TryInto};
use std::path::Path;

use anoma_shared::ledger::gas::{self, BlockGasMeter};
use anoma_shared::ledger::storage::write_log::WriteLog;
use anoma_shared::ledger::{ibc, parameters, pos};
use anoma_shared::proto::{self, Tx};
use anoma_shared::types::address::Address;
use anoma_shared::types::key::ed25519::PublicKey;
use anoma_shared::types::storage::{BlockHash, BlockHeight, Key};
use anoma_shared::types::time::{DateTimeUtc, TimeZone, Utc};
use anoma_shared::types::token::Amount;
use anoma_shared::types::{address, key, token};
use borsh::BorshSerialize;
use itertools::Itertools;
use thiserror::Error;
use tower_abci::{request, response};

use crate::node::ledger::{protocol, storage, tendermint_node};
use crate::{config, genesis, wallet};

#[derive(Error, Debug)]
pub enum Error {
    #[error("Error removing the DB data: {0}")]
    RemoveDB(std::io::Error),
    #[error("chain ID mismatch: {0}")]
    ChainIdError(String),
    #[error("Error decoding a transaction from bytes: {0}")]
    TxDecodingError(proto::Error),
    #[error("Error trying to apply a transaction: {0}")]
    TxError(protocol::Error),
    #[error("{0}")]
    Tendermint(tendermint_node::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn reset(config: config::Ledger) -> Result<()> {
    // simply nuke the DB files
    let db_path = &config.db;
    match std::fs::remove_dir_all(&db_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        res => res.map_err(Error::RemoveDB)?,
    };
    // reset Tendermint state
    tendermint_node::reset(config).map_err(Error::Tendermint)?;
    Ok(())
}

#[derive(Clone, Debug)]
pub enum MempoolTxType {
    /// A transaction that has not been validated by this node before
    NewTransaction,
    /// A transaction that has been validated at some previous level that may
    /// need to be validated again
    RecheckTransaction,
}

#[derive(Debug)]
pub struct Shell {
    storage: storage::PersistentStorage,
    gas_meter: BlockGasMeter,
    write_log: WriteLog,
}

impl Shell {
    /// Create a new shell from a path to a database and a chain id. Looks
    /// up the database with this data and tries to load the last state.
    pub fn new(db_path: impl AsRef<Path>, chain_id: String) -> Self {
        let mut storage = storage::open(db_path, chain_id);
        storage
            .load_last_state()
            .map_err(|e| {
                tracing::error!("Cannot load the last state from the DB {}", e);
            })
            .expect("PersistentStorage cannot be initialized");

        Self {
            storage,
            gas_meter: BlockGasMeter::default(),
            write_log: WriteLog::default(),
        }
    }

    /// Create a new genesis for the chain with specified id. This includes
    /// 1. A set of initial users and tokens
    /// 2. Setting up the validity predicates for both users and tokens
    /// 3. A matchmaker
    pub fn init_chain(
        &mut self,
        init: request::InitChain,
    ) -> Result<response::InitChain> {
        let response = response::InitChain::default();
        let (current_chain_id, _) = self.storage.get_chain_id();
        if current_chain_id != init.chain_id {
            return Err(Error::ChainIdError(format!(
                "Current chain ID: {}, Tendermint chain ID: {}",
                current_chain_id, init.chain_id
            )));
        }
        let genesis = genesis::genesis();

        // Initialize because there is no block
        let token_vp =
            std::fs::read("wasm/vp_token.wasm").expect("cannot load token VP");
        let user_vp =
            std::fs::read("wasm/vp_user.wasm").expect("cannot load user VP");

        // TODO load initial accounts from genesis

        // temporary account addresses for testing, generated by the
        // address.rs module
        let alberto = Address::decode("a1qq5qqqqqg4znssfsgcurjsfhgfpy2vjyxy6yg3z98pp5zvp5xgersvfjxvcnx3f4xycrzdfkak0xhx")
            .expect("The genesis address shouldn't fail decoding");
        let bertha = Address::decode("a1qq5qqqqqxv6yydz9xc6ry33589q5x33eggcnjs2xx9znydj9xuens3phxppnwvzpg4rrqdpswve4n9")
            .expect("The genesis address shouldn't fail decoding");
        let christel = Address::decode("a1qq5qqqqqxsuygd2x8pq5yw2ygdryxs6xgsmrsdzx8pryxv34gfrrssfjgccyg3zpxezrqd2y2s3g5s")
            .expect("The genesis address shouldn't fail decoding");
        let users = vec![alberto, bertha, christel];

        let tokens = vec![
            address::xan(),
            address::btc(),
            address::eth(),
            address::dot(),
            address::schnitzel(),
            address::apfel(),
            address::kartoffel(),
        ];

        for token in &tokens {
            // default tokens VPs for testing
            let key = Key::validity_predicate(&token);
            self.storage
                .write(&key, token_vp.to_vec())
                .expect("Unable to write token VP");
        }

        for (user, token) in users.iter().cartesian_product(tokens.iter()) {
            // default user VPs for testing
            self.storage
                .write(&Key::validity_predicate(user), user_vp.to_vec())
                .expect("Unable to write user VP");

            // default user's tokens for testing
            self.storage
                .write(
                    &token::balance_key(token, user),
                    Amount::whole(1_000_000)
                        .try_to_vec()
                        .expect("encode token amount"),
                )
                .expect("Unable to set genesis balance");

            // default user's public keys for testing
            let pk_key = key::ed25519::pk_key(user);
            let pk = PublicKey::from(wallet::key_of(user.encode()).public);
            self.storage
                .write(&pk_key, pk.try_to_vec().expect("encode public key"))
                .expect("Unable to set genesis user public key");
        }

        // Temporary for testing, we have a fixed matchmaker account.  This
        // account has a public key for signing matchmaker txs and verifying
        // their signatures in its VP. The VP is the same as the user's VP,
        // which simply checks the signature. We could consider using the
        // same key as the intent gossip's p2p key.
        let matchmaker = address::matchmaker();
        let matchmaker_pk = key::ed25519::pk_key(&matchmaker);
        self.storage
            .write(
                &matchmaker_pk,
                wallet::matchmaker_pk()
                    .try_to_vec()
                    .expect("encode public key"),
            )
            .expect("Unable to set genesis user public key");
        self.storage
            .write(&Key::validity_predicate(&matchmaker), user_vp.to_vec())
            .expect("Unable to write matchmaker VP");

        pos::init_genesis_storage(&mut self.storage);
        ibc::init_genesis_storage(&mut self.storage);
        parameters::init_genesis_storage(
            &mut self.storage,
            &genesis.parameters,
        );

        let ts: tendermint_proto::google::protobuf::Timestamp =
            init.time.expect("Missing genesis time");
        let initial_height = init
            .initial_height
            .try_into()
            .expect("Unexpected block height");
        // TODO hacky conversion, depends on https://github.com/informalsystems/tendermint-rs/issues/870
        let genesis_time: DateTimeUtc =
            (Utc.timestamp(ts.seconds, ts.nanos as u32)).into();

        self.storage
            .init_genesis_epoch(initial_height, genesis_time)
            .expect("Initializing genesis epoch must not fail");

        Ok(response)
    }

    /// Load the Merkle root hash and the height of the last committed block, if
    /// any. This is returned when ABCI sends an `info` request.
    pub fn last_state(&self) -> response::Info {
        let mut response = response::Info::default();
        let result = self.storage.get_state();
        match result {
            Some((root, height)) => {
                tracing::info!(
                    "Last state root hash: {}, height: {}",
                    root,
                    height
                );
                response.last_block_app_hash = root.0;
                response.last_block_height =
                    height.try_into().expect("Invalid block height");
            }
            None => {
                tracing::info!(
                    "No state could be found, chain is not initialized"
                );
            }
        };

        response
    }

    /// Uses `path` in the query to forward the request to the
    /// right query method and returns the result (which may be
    /// the default if `path` is not a supported string.
    pub fn query(&mut self, query: request::Query) -> response::Query {
        match query.path.as_str() {
            "dry_run_tx" => self.dry_run_tx(&query.data),
            _ => response::Query::default(),
        }
    }

    /// Begin a new block.
    pub fn begin_block(
        &mut self,
        hash: BlockHash,
        height: BlockHeight,
        time: DateTimeUtc,
    ) {
        self.gas_meter.reset();
        self.storage
            .begin_block(hash, height)
            .expect("Must be able to begin a block");
        self.storage
            .update_epoch(height, time)
            .expect("Must be able to update epoch");
    }

    /// Validate and apply a transaction.
    pub fn apply_tx(&mut self, req: request::DeliverTx) -> response::DeliverTx {
        let mut response = response::DeliverTx::default();
        let result = protocol::apply_tx(
            &*req.tx,
            &mut self.gas_meter,
            &mut self.write_log,
            &self.storage,
        )
        .map_err(Error::TxError);

        match result {
            Ok(result) => {
                if result.is_accepted() {
                    tracing::info!(
                        "all VPs accepted apply_tx storage modification {:#?}",
                        result
                    );
                    self.write_log.commit_tx();
                } else {
                    tracing::info!(
                        "some VPs rejected apply_tx storage modification {:#?}",
                        result.vps_result.rejected_vps
                    );
                    self.write_log.drop_tx();
                    response.code = 1;
                }
                response.gas_used = gas::as_i64(result.gas_used);
                response.info = result.to_string();
            }
            Err(msg) => {
                response.gas_used =
                    gas::as_i64(self.gas_meter.get_current_transaction_gas());
                response.info = msg.to_string();
            }
        }
        response
    }

    /// End a block.
    pub fn end_block(&mut self, _height: BlockHeight) -> response::EndBlock {
        Default::default()
    }

    /// Commit a block. Persist the application state and return the Merkle root
    /// hash.
    pub fn commit(&mut self) -> response::Commit {
        let mut response = response::Commit::default();
        // commit changes from the write-log to storage
        self.write_log
            .commit_block(&mut self.storage)
            .expect("Expected committing block write log success");
        // store the block's data in DB
        // TODO commit async?
        self.storage.commit().unwrap_or_else(|e| {
            tracing::error!(
                "Encountered a storage error while committing a block {:?}",
                e
            )
        });
        let root = self.storage.merkle_root();
        tracing::info!(
            "Committed block hash: {}, height: {}",
            root,
            self.storage.current_height,
        );
        response.data = root.0;
        response
    }

    /// Validate a transaction request. On success, the transaction will
    /// included in the mempool and propagated to peers, otherwise it will be
    /// rejected.
    pub fn mempool_validate(
        &self,
        tx_bytes: &[u8],
        r#_type: MempoolTxType,
    ) -> response::CheckTx {
        let mut response = response::CheckTx::default();
        match Tx::try_from(tx_bytes).map_err(Error::TxDecodingError) {
            Ok(_) => response.info = String::from("Mempool validation passed"),
            Err(msg) => {
                response.code = 1;
                response.log = msg.to_string();
            }
        }
        response
    }

    /// Simulate validation and application of a transaction.
    fn dry_run_tx(&mut self, tx_bytes: &[u8]) -> response::Query {
        let mut response = response::Query::default();
        let mut gas_meter = BlockGasMeter::default();
        let mut write_log = self.write_log.clone();
        match protocol::apply_tx(
            tx_bytes,
            &mut gas_meter,
            &mut write_log,
            &self.storage,
        )
        .map_err(Error::TxError)
        {
            Ok(result) => response.info = result.to_string(),
            Err(error) => {
                response.code = 1;
                response.log = format!("{}", error);
            }
        }
        response
    }
}
