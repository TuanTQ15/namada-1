use std::collections::HashSet;
use std::env;
use std::path::Path;
use std::rc::Rc;

use anoma::proto::{Intent, IntentId, Tx};
use anoma::types::address::{xan, Address};
use anoma::types::dylib;
use anoma::types::intent::{IntentTransfers, MatchedExchanges};
use anoma::types::key::ed25519::Keypair;
use anoma::types::matchmaker::AddIntentResult;
use anoma::types::transaction::{Fee, WrapperTx};
use anoma::vm::wasm;
use borsh::{BorshDeserialize, BorshSerialize};
use libloading::Library;
#[cfg(not(feature = "ABCI"))]
use tendermint_config::net;
#[cfg(feature = "ABCI")]
use tendermint_config_abci::net;
use thiserror::Error;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::oneshot;

use super::filter::Filter;
use super::mempool::{self, IntentMempool};
use crate::cli::args;
use crate::client::rpc;
use crate::client::tx::broadcast_tx;
use crate::{config, wasm_loader};

/// A matchmaker receive intents and tries to find a match with previously
/// received intent.
#[derive(Debug)]
pub struct Matchmaker {
    /// Matchmaker's implementation loaded from dylib
    matchmaker_code: Library,
    /// All valid and received intent are saved in this mempool
    mempool: IntentMempool,
    /// Possible filter that filter any received intent.
    filter: Option<Filter>,
    /// The code of the transaction that is going to be send to a ledger.
    tx_code: Vec<u8>,
    /// the matchmaker's state as arbitrary bytes
    state: Vec<u8>,
    /// The ledger address to send any crafted transaction to
    ledger_address: net::Address,
    /// A source address for transactions created from intents.
    tx_source_address: Address,
    /// A keypair that will be used to sign transactions.
    tx_signing_key: Rc<Keypair>,
}

/// Matchmaker message for communication between the runner, P2P and the
/// implementation
#[derive(Debug)]
pub enum MatchmakerMessage {
    /// Run the matchmaker with the given intent
    ApplyIntent(Intent, oneshot::Sender<bool>),
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed to add intent to mempool: {0}")]
    MempoolFailed(mempool::Error),
    #[error("Failed to run matchmaker prog: {0}")]
    RunnerFailed(wasm::run::Error),
    #[error("Failed to read file: {0}")]
    FileFailed(std::io::Error),
    #[error("Failed to create filter: {0}")]
    FilterInit(super::filter::Error),
    #[error("Failed to run filter: {0}")]
    Filter(super::filter::Error),
}

type Result<T> = std::result::Result<T, Error>;

impl Matchmaker {
    /// Create a new matchmaker based on the parameter config.
    pub fn new(
        config: &config::Matchmaker,
        wasm_dir: impl AsRef<Path>,
        tx_source_address: Address,
        tx_signing_key: Rc<Keypair>,
    ) -> Result<(Self, Sender<MatchmakerMessage>, Receiver<MatchmakerMessage>)>
    {
        // TODO: find a good number or maybe unlimited channel ?
        let (sender, receiver) = channel(100);

        // The dylib should be built in the same directory as where Anoma
        // binaries are, even when ran via `cargo run`. Anoma's pre-built
        // binaries are distributed with the dylib(s) in the same directory.
        let dylib_dir = {
            let anoma_path = env::current_exe().unwrap();
            anoma_path
                .parent()
                .map(|path| path.to_owned())
                .unwrap_or_else(|| ".".into())
        };
        let mut matchmaker_dylib = dylib_dir.join(&config.matchmaker);
        matchmaker_dylib.set_extension(dylib::FILE_EXT);
        tracing::info!(
            "Running matchmaker from {}",
            matchmaker_dylib.to_string_lossy()
        );
        if !matchmaker_dylib.exists() {
            panic!(
                "The matchmaker library couldn't not be found. Did you build \
                 it?"
            )
        }
        let matchmaker_code =
            unsafe { Library::new(matchmaker_dylib).unwrap() };

        let tx_code = wasm_loader::read_wasm(&wasm_dir, &config.tx_code);
        let filter = config
            .filter
            .as_ref()
            .map(Filter::from_file)
            .transpose()
            .map_err(Error::FilterInit)?;

        Ok((
            Self {
                mempool: IntentMempool::new(),
                filter,
                matchmaker_code,
                tx_code,
                state: Vec::new(),
                ledger_address: config.ledger_address.clone(),
                tx_source_address,
                tx_signing_key,
            },
            sender,
            receiver,
        ))
    }

    /// Tries to apply the filter or returns true if no filter is define
    fn apply_filter(&self, intent: &Intent) -> Result<bool> {
        self.filter
            .as_ref()
            .map(|f| f.validate(intent))
            .transpose()
            .map(|v| v.unwrap_or(true))
            .map_err(Error::Filter)
    }

    /// add the intent to the matchmaker mempool and tries to find a match for
    /// that intent
    pub async fn try_match_intent(&mut self, intent: &Intent) -> Result<bool> {
        Ok(if self.apply_filter(intent)? {
            self.mempool
                .put(intent.clone())
                .map_err(Error::MempoolFailed)?;

            let add_intent: libloading::Symbol<
                unsafe extern "C" fn(
                    &Vec<u8>,
                    &Vec<u8>,
                    &Vec<u8>,
                ) -> AddIntentResult,
            > = unsafe { self.matchmaker_code.get(b"add_intent").unwrap() };

            let _ = anoma::types::key::ed25519::Signed::<
                anoma::types::intent::FungibleTokenIntent,
            >::try_from_slice(&intent.data)
            .unwrap();
            let result = unsafe {
                add_intent(&self.state, &intent.id().0, &intent.data)
            };

            if let Some(tx) = result.tx {
                self.submit_tx(tx).await
            }
            if let Some(matched_intents) = result.matched_intents {
                self.removed_intents(matched_intents)
            }
            self.state = result.state;

            true
        } else {
            false
        })
    }

    async fn submit_tx(&self, tx_data: Vec<u8>) {
        let tx_code = self.tx_code.clone();
        let matches = MatchedExchanges::try_from_slice(&tx_data[..]).unwrap();
        let intent_transfers = IntentTransfers {
            matches,
            source: self.tx_source_address.clone(),
        };
        let tx_data = intent_transfers.try_to_vec().unwrap();
        let tx = WrapperTx::new(
            Fee {
                amount: 0.into(),
                token: xan(),
            },
            &self.tx_signing_key,
            rpc::query_epoch(args::Query {
                ledger_address: self.ledger_address.clone(),
            })
            .await,
            0.into(),
            Tx::new(tx_code, Some(tx_data)).sign(&self.tx_signing_key),
        );

        let response =
            broadcast_tx(self.ledger_address.clone(), tx, &self.tx_signing_key)
                .await;
        match response {
            Ok(tx_response) => {
                tracing::info!(
                    "Injected transaction from matchmaker with result: {:#?}",
                    tx_response
                );
            }
            Err(err) => {
                tracing::error!(
                    "Matchmaker error in submitting a transaction to the \
                     ledger: {}",
                    err
                );
            }
        }
    }

    fn removed_intents(&mut self, intent_ids: HashSet<Vec<u8>>) {
        intent_ids.into_iter().for_each(|intent_id| {
            self.mempool.remove(&IntentId::from(intent_id));
        });
    }

    pub async fn handle_mm_message(&mut self, mm_message: MatchmakerMessage) {
        match mm_message {
            MatchmakerMessage::ApplyIntent(intent, response_sender) => {
                let result = self
                    .try_match_intent(&intent)
                    .await
                    .unwrap_or_else(|err| {
                        tracing::error!(
                            "Matchmaker error in applying intent {}",
                            err
                        );
                        false
                    });
                response_sender.send(result).unwrap_or_else(|err| {
                    tracing::error!(
                        "Matchmaker error in sending back intent result {}",
                        err
                    )
                });
            }
        }
    }
}
