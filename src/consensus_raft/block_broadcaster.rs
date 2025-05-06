use std::{io::{Error, ErrorKind}, sync::Arc};

use log::{debug, error, info, trace};
use prost::Message;
use rustls::crypto;
use tokio::sync::{oneshot, Mutex};

use crate::{config::AtomicConfig, crypto::{AtomicKeyStore, FutureHash, HashType}, proto::{consensus::{HalfSerializedBlock, ProtoAppendEntries, ProtoBlock, ProtoFork, ProtoRaftAppendEntries}, execution::ProtoTransaction, rpc::ProtoPayload}, rpc::{client::{Client, PinnedClient}, server::LatencyProfile, PinnedMessage, SenderType}, utils::{channel::{Receiver, Sender}, RaftStorageServiceConnector, StorageAck}};

use super::app::AppCommand;
use super::batch_proposal::{MsgAckChanWithTag, RawBatch};

pub enum BlockBroadcasterCommand {
    UpdateCI(u64),
    NextAEForkPrefix(Vec<oneshot::Receiver<Result<ProtoBlock, Error>>>),
}

pub struct BlockBroadcaster {
    config: AtomicConfig,
    ci: u64,
    fork_prefix_buffer: Vec<ProtoBlock>,
    
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
            fork_prefix_buffer: Vec::new(),
            my_block_rx,
            control_command_rx,
            storage,
            client,
            staging_tx,
        }
    }

    pub async fn run(block_broadcaster: Arc<Mutex<Self>>) {
        let mut block_broadcaster = block_broadcaster.lock().await;

        let mut total_work = 0;
        loop {
            if let Err(_e) = block_broadcaster.worker().await {
                break;
            }

            total_work += 1;
        }

        info!("Broadcasting worker exited.");
    }

    async fn worker(&mut self) -> Result<(), Error> {
        tokio::select! {
            _batch_and_client_reply = self.my_block_rx.recv() => {
                if let Some(_) = _batch_and_client_reply {
                    let (batch, client_reply) = _batch_and_client_reply.unwrap();
                    self.process_my_block(batch, client_reply).await;
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
            BlockBroadcasterCommand::NextAEForkPrefix(blocks) => {
                for block in blocks {
                    let block = block.await.unwrap().expect("Failed to get block");
                    self.fork_prefix_buffer.push(block);
                }
            }
        }

        Ok(())
    }

    async fn store_and_forward_internally(&mut self, block: &ProtoBlock, this_is_final_block: bool) -> Result<(), Error> {
        // Store
        let storage_ack = self.storage.put_block(block).await;
        // info!("Sending {}", block.block.n);
        self.staging_tx.send((block.clone(), storage_ack, ae_stats, this_is_final_block)).await.unwrap();
        Ok(())
    }

    async fn process_my_block(&mut self, batch: RawBatch, client_reply: Vec<MsgAckChanWithTag>) -> Result<(), Error> {
        let config = self.config.get();
        // TODO: Replace ProtoBlock with simpler Block
        let block = ProtoBlock {
            n,
            parent: Vec::new(),
            view: 0,
            qc: qc_list,
            fork_validation,
            view_is_stable: self.view_is_stable,
            config_num: self.config_num,
            tx_list: batch,
            sig: None,
        };
        
        let (view, config_num) = (block.view, block.config_num);
        // First forward all blocks that were in the fork prefix buffer.
        let mut ae_fork = Vec::new();

        for block in self.fork_prefix_buffer.drain(..) {
            ae_fork.push(block);
        }
        ae_fork.push(block.clone());

        if ae_fork.len() > 1 {
            trace!("AE: {:?}", ae_fork);
        }

        let _fork_size = ae_fork.len();
        let mut cnt = 0;
        for block in &ae_fork {
            cnt += 1;
            let this_is_final_block = cnt == _fork_size;
            self.store_and_forward_internally(&block, this_is_final_block).await?;
        }
        // Forward to other nodes. Involves copies and serialization so done last.

        let names = self.get_everyone_except_me();
        self.broadcast_ae_fork(names, ae_fork, view, view_is_stable, config_num).await;

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

    async fn broadcast_ae_fork(&mut self, names: Vec<String>, mut ae_fork: Vec<ProtoBlock>, view: u64, view_is_stable: bool, config_num: u64) {        
        let append_entry = ProtoAppendEntries {
            fork: Some(ProtoFork {
                serialized_blocks: ae_fork.drain(..).map(|block| HalfSerializedBlock { 
                    n: block.block.n,
                    view: block.block.view,
                    view_is_stable: block.block.view_is_stable,
                    config_num: block.block.config_num,
                    serialized_body: block.block_ser.clone(), 
                }).collect(),
            }),
            commit_index: self.ci,
            view,
            view_is_stable,
            config_num,
            is_backfill_response: false,
        };
        // let append_entry = ProtoRaftAppendEntries {
        //     term: view,
        //     leader_id: self.config.get().net_config.name.clone(), // TODO: Use correct name
        //     prev_log_index: 0,
        //     prev_log_term: 0,
        //     entries: vec![],
        //     leader_commit: self.ci,
        // };

        let rpc = ProtoPayload {
            message: Some(crate::proto::rpc::proto_payload::Message::AppendEntries(append_entry)),
            // message: Some(crate::proto::rpc::proto_payload::Message::RaftAppendEntries(append_entry)),
        };
        let data = rpc.encode_to_vec();

        let sz = data.len();
        if !view_is_stable {
            info!("AE size: {} Broadcasting to {:?}", sz, names);
        }
        let data = PinnedMessage::from(data, sz, SenderType::Anon);
        let mut profile = LatencyProfile::new();
        let _res = PinnedClient::broadcast(&self.client, &names, &data, &mut profile, get_broadcast_threshold()).await;

    }
}