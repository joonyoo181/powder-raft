use std::io::Error;

use tokio::sync::oneshot;

use crate::crypto::HashType;

use super::{channel::{make_channel, Receiver, Sender}, StorageEngine};

enum RaftStorageServiceCommand {
    Put(HashType /* key */, Vec<u8> /* val */, oneshot::Sender<Result<(), Error>>),
    Get(HashType /* key */, oneshot::Sender<Result<Vec<u8>, Error>>)
}

pub struct RaftStorageService<S: StorageEngine> {
    db: S,

    cmd_rx: Receiver<RaftStorageServiceCommand>,
    cmd_tx: Sender<RaftStorageServiceCommand>
}


pub struct RaftStorageServiceConnector {
    cmd_tx: Sender<RaftStorageServiceCommand>,
}


impl<S: StorageEngine> RaftStorageService<S> {
    pub fn new(db: S, buffer_size: usize) -> Self {
        let (cmd_tx, cmd_rx) = make_channel(buffer_size);
        Self { db, cmd_rx, cmd_tx }
    }

    pub fn get_connector(&self) -> RaftStorageServiceConnector {
        RaftStorageServiceConnector {
            cmd_tx: self.cmd_tx.clone(),
        }
    }

    pub async fn run(&mut self) {
        self.db.init();
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                RaftStorageServiceCommand::Put(key, val, ok_chan) => {
                    #[cfg(feature = "storage")]
                    {
                        let res = self.db.put_block(&val, &key);
                        let _ = ok_chan.send(res);
                    }

                    #[cfg(not(feature = "storage"))]
                    let _ = ok_chan.send(Ok(()));
                },
                RaftStorageServiceCommand::Get(key, val_chan) => {
                    let res = self.db.get_block(&key);
                    let _ = val_chan.send(res);
                },
            }
        }
        self.db.destroy();
    }
}

pub type StorageAck = Result<(), Error>;

impl RaftStorageServiceConnector {
    pub async fn get_block(&mut self, block_hash: &HashType) -> Result<ProtoBlock, Error> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(RaftStorageServiceCommand::Get(block_hash.clone(), tx)).await.unwrap();
    }

    pub async fn put_block(&self, block: &ProtoBlock) -> oneshot::Receiver<StorageAck> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(RaftStorageServiceCommand::Put(block.block_hash.clone(), block.block_ser.clone(), tx)).await.unwrap();

        rx
    }

    pub async fn put_raw(&self, key: String, val: Vec<u8>) -> oneshot::Receiver<StorageAck> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(RaftStorageServiceCommand::Put(key.into_bytes(), val, tx)).await.unwrap();

        rx
    }

    pub async fn get_raw(&self, key: String) -> Result<Vec<u8>, Error> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(RaftStorageServiceCommand::Get(key.into_bytes(), tx)).await.unwrap();

        rx.await.unwrap()
    }
}
