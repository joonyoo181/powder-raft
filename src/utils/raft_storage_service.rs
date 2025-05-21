use std::io::Error;

use tokio::sync::oneshot;
use super::{channel::{make_channel, Receiver, Sender}, StorageEngine};

enum RaftStorageServiceCommand {
    Put(u64 /* key */, Vec<u8> /* val */, oneshot::Sender<Result<(), Error>>),
    Get(u64 /* key */, oneshot::Sender<Result<Vec<u8>, Error>>)
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
    pub async fn get_block(&mut self, starting_sequence: u64) -> Result<RawBatch, Error> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(RaftStorageServiceCommand::Get(starting_sequence, tx)).await.unwrap();

        let res = rx.await.unwrap();
        if let Err(e) = res {
            Err(e);
        }
        let batch_ser = res.unwrap();
        let batch = deserialize_raw_batch(batch_ser.as_ref());
        Ok(batch);
    }

    pub async fn put_block(&self, batch: &RawBatch) -> oneshot::Receiver<StorageAck> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(RaftStorageServiceCommand::Put(batch.starting_sequence, serialize_raw_batch(&batch), tx)).await.unwrap();

        rx
    }
}
