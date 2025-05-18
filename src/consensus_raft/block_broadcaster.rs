use std::{io::{Error, ErrorKind}, sync::Arc, vec};

use log::{debug, error, info, trace};
use prost::Message;
use rustls::crypto;
use tokio::sync::{oneshot, Mutex};

use crate::{config::AtomicConfig, crypto::{AtomicKeyStore, FutureHash, HashType}, proto::{consensus::{HalfSerializedBlock, ProtoAppendEntries, ProtoBlock, ProtoRaftAppendEntries}, execution::ProtoTransaction, rpc::ProtoPayload}, rpc::{client::{Client, PinnedClient}, server::LatencyProfile, PinnedMessage, SenderType}, utils::{channel::{Receiver, Sender}, RaftStorageServiceConnector, StorageAck}};

use super::{app::AppCommand, RaftLogEntry};
use super::batch_proposal::{MsgAckChanWithTag, RawBatch};

pub enum BlockBroadcasterCommand {
    UpdateCI(u64),
}

pub struct BlockBroadcaster {
    config: AtomicConfig,
    ci: u64,
    
    // Input ports
    my_block_rx: Receiver<(RawBatch, Vec<MsgAckChanWithTag>)>,
    control_command_rx: Receiver<BlockBroadcasterCommand>,
    
    // Output ports
    storage: RaftStorageServiceConnector,
    client: PinnedClient,
    staging_tx: Sender<(ProtoBlock, oneshot::Receiver<StorageAck>, bool /* this_is_final_block */)>,
}

impl BlockBroadcaster {
    pub fn new(
        config: AtomicConfig,
        client: PinnedClient,
        my_block_rx: Receiver<(RawBatch, Vec<MsgAckChanWithTag>)>,
        control_command_rx: Receiver<BlockBroadcasterCommand>,
        storage: RaftStorageServiceConnector,
        staging_tx: Sender<(ProtoBlock, oneshot::Receiver<StorageAck>, bool)>,
    ) -> Self {

        let my_block_event_order = vec![
            "Retrieve prepared block",
            "Store block",
            "Forward block to logserver",
            "Forward block to staging",
            "Serialize",
            "Forward block to other nodes",
        ];

        Self {
            config,
            ci: 0,
            my_block_rx,
            control_command_rx,
            storage,
            client,
            staging_tx,
        }
    }

    pub async fn run(block_broadcaster: Arc<Mutex<Self>>, raft_state: Arc<Mutex<RaftState>>) {
        let mut block_broadcaster = block_broadcaster.lock().await;
        let mut raft_state = raft_state.lock().await;

        let mut total_work = 0;
        loop {
            if let Err(_e) = block_broadcaster.worker(raft_state).await {
                break;
            }

            total_work += 1;
        }

        info!("Broadcasting worker exited.");
    }

    async fn worker(&mut self, raft_state: MutexGuard<'_, RaftState>) -> Result<(), Error> {
        tokio::select! {
            _batch_and_client_reply = self.my_block_rx.recv() => {
                if let Some(_) = _batch_and_client_reply {
                    let (batch, client_reply) = _batch_and_client_reply.unwrap();
                    self.process_my_block(batch, client_reply, raft_state).await;
                }
            }

            cmd = self.control_command_rx.recv() => {
                if cmd.is_none() {
                    return Err(Error::new(ErrorKind::BrokenPipe, "control_command_rx channel closed"));
                }
                self.handle_control_command(cmd.unwrap()).await?;
            }
        }

        Ok(())
    }

    fn get_everyone_except_me(&self) -> Vec<String> {
        let config = self.config.get();
        let me = &config.net_config.name;
        let mut node_list = config.consensus_config.node_list
            .iter().filter(|e| *e != me).map(|e| e.clone())
            .collect::<Vec<_>>();

        node_list.extend(
            config.consensus_config.learner_list.iter().map(|e| e.clone())
        );

        node_list
    }

    async fn handle_control_command(&mut self, cmd: BlockBroadcasterCommand) -> Result<(), Error> {
        match cmd {
            BlockBroadcasterCommand::UpdateCI(ci) => self.ci = ci,
        }

        Ok(())
    }

    async fn store_and_forward_internally(&mut self, block: &ProtoBlock, this_is_final_block: bool) -> Result<(), Error> {
        // Store
        let storage_ack = self.storage.put_block(block).await;
        // info!("Sending {}", block.block.n);
        self.staging_tx.send((block.clone(), storage_ack, this_is_final_block)).await.unwrap();
        Ok(())
    }

    async fn process_my_block(&mut self, batch: RawBatch, client_reply: Vec<MsgAckChanWithTag>, raft_state: MutexGuard<'_, RaftState>) -> Result<(), Error> {
        let new_entries = Vec::new();
        for xact in batch {
            new_entries.push(RaftLogEntry {
                term: raft_state.current_term,
                command: xact.clone(),
            });
        }

        raft_state.log.extend(new_entries.clone());

        let names = self.get_everyone_except_me();
        self.broadcast_ae(names, raft_state, new_entries).await;

        Ok(())
    }

    fn get_broadcast_threshold(&self) -> usize {
        let config = self.config.get();
        let node_list_len = config.consensus_config.node_list.len();
        
        #[cfg(feature = "no_qc")]
        {
            let f = node_list_len / 2;
            return f;
        }

        #[cfg(feature = "platforms")]
        {
            if node_list_len <= config.consensus_config.liveness_u as usize {
                return 0;
            }
            let byzantine_threshold = node_list_len - config.consensus_config.liveness_u as usize;
            return byzantine_threshold - 1;
        }

        let f = node_list_len / 3;
        return 2 * f;


    }

    async fn broadcast_ae(&mut self, names: Vec<String>, raft_state: MutexGuard<'_, RaftState>, new_entries: Vec<RaftLogEntry>) {        
        let leader_id = self.config.get().consensus_config.get_leader_for_view(raft_state.current_term);

        let prev_log_index = max(raft_state.log.len() - new_entries.len(), 0);
        let prev_log_term = if raft_state.log.len() > 0 {
            raft_state.log[prev_log_index].term
        } else {
            0
        };

        let raft_append_entry = ProtoRaftAppendEntries {
            term: raft_state.current_term,
            leader_id,
            prev_log_index: prev_log_index as u64,
            prev_log_term,
            entries: new_entries.clone(),
            leader_commit: self.ci,
        };

        let rpc = ProtoPayload {
            message: Some(crate::proto::rpc::proto_payload::Message::RaftAppendEntries(raft_append_entry)),
        };
        let data = rpc.encode_to_vec();
        let sz = data.len();
        let data = PinnedMessage::from(data, sz, SenderType::Anon);
        let mut profile = LatencyProfile::new();
        let _res = PinnedClient::broadcast(&self.client, &names, &data, &mut profile, self.get_broadcast_threshold()).await;

    }
}