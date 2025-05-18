pub mod batch_proposal;
mod block_broadcaster;
pub mod app;
pub mod engines;
pub mod client_reply;

use std::{io::{Error, ErrorKind}, ops::Deref, pin::Pin, sync::Arc};

use app::{AppEngine, Application};
use batch_proposal::{BatchProposer, TxWithAckChanTag};
use block_broadcaster::BlockBroadcaster;
use log::{debug, info, warn};
use prost::Message;
use tokio::{sync::{mpsc::unbounded_channel, Mutex}, task::JoinSet};
use crate::{proto::{checkpoint::ProtoBackfillNack, consensus::{ProtoAppendEntries, ProtoRaftAppendEntries, ProtoRaftAppendEntriesReply, ProtoViewChange}, execution::ProtoTransaction}, rpc::{client::Client, SenderType}, utils::{channel::{make_channel, Receiver, Sender}, RaftStorageService, RocksDBStorageEngine}};

use crate::{config::{AtomicConfig, Config}, crypto::{AtomicKeyStore, CryptoService, KeyStore}, proto::rpc::ProtoPayload, rpc::{server::{MsgAckChan, RespType, Server, ServerContextType}, MessageRef}};

pub struct ConsensusServerContext {
    config: AtomicConfig,
    keystore: AtomicKeyStore,
    batch_proposal_tx: Sender<TxWithAckChanTag>,
    raft_ae_tx: Sender<ProtoRaftAppendEntries>,
    raft_ae_reply_tx: Sender<ProtoRaftAppendEntriesReply>,
}


#[derive(Clone)]
pub struct PinnedConsensusServerContext(pub Arc<Pin<Box<ConsensusServerContext>>>);

impl PinnedConsensusServerContext {
    pub fn new(
        config: AtomicConfig, keystore: AtomicKeyStore,
        batch_proposal_tx: Sender<TxWithAckChanTag>,
        raft_ae_tx: Sender<ProtoRaftAppendEntries>,
        raft_ae_reply_tx: Sender<ProtoRaftAppendEntriesReply>,
    ) -> Self {
        Self(Arc::new(Box::pin(ConsensusServerContext {
            config, keystore,
            batch_proposal_tx,
            raft_ae_tx,
            raft_ae_reply_tx
        })))
    }
}

impl Deref for PinnedConsensusServerContext {
    type Target = ConsensusServerContext;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}


impl ServerContextType for PinnedConsensusServerContext {
    fn get_server_keys(&self) -> std::sync::Arc<Box<crate::crypto::KeyStore>> {
        self.keystore.get()
    }

    async fn handle_rpc(&self, m: MessageRef<'_>, ack_chan: MsgAckChan) -> Result<RespType, Error> {
        let sender = match m.2 {
            crate::rpc::SenderType::Anon => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "unauthenticated message",
                )); // Anonymous replies shouldn't come here
            }
            _sender @ crate::rpc::SenderType::Auth(_, _) => _sender.clone()
        };
        let body = match ProtoPayload::decode(&m.0.as_slice()[0..m.1]) {
            Ok(b) => b,
            Err(e) => {
                warn!("Parsing problem: {} ... Dropping connection", e.to_string());
                debug!("Original message: {:?} {:?}", &m.0, &m.1);
                return Err(Error::new(ErrorKind::InvalidData, e));
            }
        };
    
        let msg = match body.message {
            Some(m) => m,
            None => {
                warn!("Nil message: {}", m.1);
                return Ok(RespType::NoResp);
            }
        };

        match msg {
            crate::proto::rpc::proto_payload::Message::ClientRequest(proto_client_request) => {
                let client_tag = proto_client_request.client_tag;
                self.batch_proposal_tx.send((proto_client_request.tx, (ack_chan, client_tag, sender))).await
                    .expect("Channel send error");

                return Ok(RespType::Resp);
            },
            crate::proto::rpc::proto_payload::Message::RaftAppendEntries(proto_raft_append_entry) => {
                self.raft_ae_tx.send(proto_raft_append_entry).await
                    .expect("Channel send error");
                return Ok(RespType::Resp);
            },
            crate::proto::rpc::proto_payload::Message::RaftAppendEntriesReply(proto_raft_append_entry_reply) => {
                self.raft_ae_reply_tx.send(proto_raft_append_entry_reply).await
                    .expect("Channel send error");
                return Ok(RespType::Resp);
            },
            crate::proto::rpc::proto_payload::Message::ViewChange(proto_view_change) => {},
            crate::proto::rpc::proto_payload::Message::AppendEntries(proto_append_entries) => {},
            crate::proto::rpc::proto_payload::Message::Vote(proto_vote) => {},
            crate::proto::rpc::proto_payload::Message::BackfillRequest(proto_back_fill_request) => {},
            crate::proto::rpc::proto_payload::Message::BackfillResponse(proto_back_fill_response) => {},
            crate::proto::rpc::proto_payload::Message::BackfillNack(proto_backfill_nack) => {},
        }

        Ok(RespType::NoResp)
    }
}

pub struct RaftState {
    pub current_term: u64,
    pub voted_for: Option<String>,
    pub log: Vec<RaftLogEntry>,
    pub commit_index: u64,
    pub last_applied: u64,
}

pub struct RaftLogEntry {
    pub term: u64,
    pub command: ProtoTransaction,
}

pub struct ConsensusNode {
    config: AtomicConfig,
    keystore: AtomicKeyStore,

    server: Arc<Server<PinnedConsensusServerContext>>,
    storage: Arc<Mutex<RaftStorageService<RocksDBStorageEngine>>>,
    raft_state: Arc<Mutex<RaftState>>,

    /// This will be owned by the task that runs batch_proposer
    /// So the lock will be taken exactly ONCE and held forever.
    batch_proposer: Arc<Mutex<BatchProposer>>,
    block_broadcaster: Arc<Mutex<BlockBroadcaster>>,

    /// Use this to feed transactions from within the same process.
    pub batch_proposer_tx: Sender<TxWithAckChanTag>,
}

impl ConsensusNode {
    pub fn new(config: Config) -> Self {
        let (batch_proposer_tx, batch_proposer_rx) = make_channel(config.rpc_config.channel_depth as usize);
        Self::mew(config, batch_proposer_tx, batch_proposer_rx)
    }
    
    /// mew() must be called from within a Tokio context with channel passed in.
    /// This is new()'s cat brother.
    ///
    ///  /\_/\
    /// ( o.o )
    ///  > ^ < 
    pub fn mew(config: Config, batch_proposer_tx: Sender<TxWithAckChanTag>, batch_proposer_rx: Receiver<TxWithAckChanTag>) -> Self {
        let _chan_depth = config.rpc_config.channel_depth as usize;

        let key_store = KeyStore::new(
            &config.rpc_config.allowed_keylist_path,
            &config.rpc_config.signing_priv_key_path,
        );
        let config = AtomicConfig::new(config);
        let keystore = AtomicKeyStore::new(key_store);
        let storage_config = &config.get().consensus_config.log_storage_config;
        let storage = match storage_config {
            rocksdb_config @ crate::config::StorageConfig::RocksDB(_) => {
                let _db = RocksDBStorageEngine::new(rocksdb_config.clone());
                RaftStorageService::new(_db, _chan_depth)
            },
            crate::config::StorageConfig::FileStorage(_) => {
                panic!("File storage not supported!");
            },
        };
        let client = Client::new_atomic(config.clone(), keystore.clone(), false, 0);

        let (block_broadcaster_tx, block_broadcaster_rx) = make_channel(_chan_depth);
        let (client_reply_command_tx, client_reply_command_rx) = make_channel(_chan_depth);
        let (unlogged_tx, unlogged_rx) = make_channel(_chan_depth);
        let (batch_proposer_command_tx, batch_proposer_command_rx) = make_channel(_chan_depth);
        let (broadcaster_control_command_tx, broadcaster_control_command_rx) = make_channel(_chan_depth);
        let (staging_tx, staging_rx) = make_channel(_chan_depth);

        let block_broadcaster_storage = storage.get_connector();

        let batch_proposer = BatchProposer::new(config.clone(), batch_proposer_rx, block_broadcaster_tx, client_reply_command_tx.clone(), unlogged_tx, batch_proposer_command_rx);
        let block_broadcaster = BlockBroadcaster::new(config.clone(), client.into(), block_broadcaster_rx, broadcaster_control_command_rx, block_broadcaster_storage, staging_tx);

        let (raft_ae_tx, raft_ae_rx) = make_channel(_chan_depth);
        let (raft_ae_reply_tx, raft_ae_reply_rx) = make_channel(_chan_depth);

        let ctx = PinnedConsensusServerContext::new(config.clone(), keystore.clone(), batch_proposer_tx.clone(), raft_ae_tx.clone(), raft_ae_reply_tx.clone());

        Self {
            config: config.clone(),
            keystore: keystore.clone(),
            server: Arc::new(Server::new_atomic(config.clone(), ctx, keystore.clone())),
            batch_proposer: Arc::new(Mutex::new(batch_proposer)),
            block_broadcaster: Arc::new(Mutex::new(block_broadcaster)),

            storage: Arc::new(Mutex::new(storage)),
            raft_state: Arc::new(Mutex::new(RaftState {
                current_term: 0,
                voted_for: None,
                log: vec![],
                commit_index: 0,
                last_applied: 0
            })),

            batch_proposer_tx,
        }
    }

    pub async fn run(&mut self) -> JoinSet<()> {
        let server = self.server.clone();
        let raft_state = self.raft_state.clone();
        let batch_proposer = self.batch_proposer.clone();
        let storage = self.storage.clone();
        let block_broadcaster = self.block_broadcaster.clone();

        let mut handles = JoinSet::new();

        handles.spawn(async move {
            let mut storage = storage.lock().await;
            storage.run().await;
        });

        handles.spawn(async move {
            let _ = Server::<PinnedConsensusServerContext>::run(server).await;
        });

        handles.spawn(async move {
            BatchProposer::run(batch_proposer).await;
        });

        handles.spawn(async move {
            BlockBroadcaster::run(block_broadcaster, raft_state).await;
        });
    
        handles
    }
}
