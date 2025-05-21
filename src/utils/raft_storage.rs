use rocksdb::{DBCompactionStyle, FifoCompactOptions, Options, UniversalCompactOptions, WriteBatchWithTransaction, WriteOptions, DB};

use crate::config::{RocksDBConfig, StorageConfig};
use std::{fmt::Debug, io::{Error, ErrorKind}};

pub trait RaftStorageEngine: Debug + Sync + Send {
    fn init(&mut self);
    fn destroy(&self);

    /// Can't trust the storage to handle anything more than block hashes
    fn put_block(&self, block_ser: &Vec<u8>, batch_seq: u64) -> Result<(), Error>;
    fn put_multiple_blocks(&self, blocks: &Vec<(Vec<u8> /* block_ser */, u64 /* Starting sequence number */)>) -> Result<(), Error>;
    fn get_block(&self, batch_sequence: u64) -> Result<Vec<u8>, Error>;
}

#[derive(Debug)]
pub struct RocksDBRaftStorageEngine {
    pub config: RocksDBConfig,
    pub db: DB
}

impl RocksDBRaftStorageEngine {
    pub fn new(config: StorageConfig) -> RocksDBRaftStorageEngine {
        if let StorageConfig::RocksDB(config) = config {
            let mut opts = Options::default();
            opts.create_if_missing(true);
            opts.set_write_buffer_size(config.write_buffer_size);
            opts.set_max_write_buffer_number(config.max_write_buffer_number);
            opts.set_min_write_buffer_number_to_merge(config.max_write_buffers_to_merge);
            opts.set_target_file_size_base(config.write_buffer_size as u64);

            opts.set_manual_wal_flush(true);
            opts.set_compaction_style(DBCompactionStyle::Universal);
            opts.set_allow_mmap_reads(true);
            opts.set_allow_mmap_writes(true);

            // opts.increase_parallelism(3);

            let path = config.db_path.clone();
            RocksDBRaftStorageEngine {
                config,
                db: DB::open(&opts, path).unwrap(),
            }
        } else {
            panic!("Wrong config")
        }
    }
}

impl RaftStorageEngine for RocksDBRaftStorageEngine {
    fn init(&mut self) {
        // This does nothing for RocksDBRaftStorageEngine, since it is already created when new() is called.
    }

    fn destroy(&self) {
        let _ = self.db.flush();
        let mut opts = Options::default();
        opts.set_write_buffer_size(self.config.write_buffer_size);
        opts.set_max_write_buffer_number(self.config.max_write_buffer_number);
        opts.set_min_write_buffer_number_to_merge(self.config.max_write_buffers_to_merge);

        #[cfg(not(feature = "disk_wal"))]
        opts.set_manual_wal_flush(true);


        opts.set_compaction_style(DBCompactionStyle::Universal);

        let _ = DB::destroy(&opts, &self.config.db_path);
    }

    fn put_block(&self, block_ser: &Vec<u8>, batch_seq: u64) -> Result<(), Error> {
        let mut wopts = WriteOptions::default();


        // #[cfg(feature = "disk_wal")]
        // wopts.set_sync(true);

        #[cfg(not(feature = "disk_wal"))]
        wopts.disable_wal(true);
        
        let res = // self.db.put(block_hash, block_ser);
            self.db.put_opt(batch_seq, block_ser, &wopts);
        match res {
            Ok(_) => {
                return Ok(())
            },
            Err(e) => {
                return Err(Error::new(ErrorKind::BrokenPipe, e))
            },
        }
    }

    fn get_block(&self, batch_seq: u64) -> Result<Vec<u8>, Error> {
        let res = self.db.get(batch_seq);
        match res {
            Ok(val) => {
                match val {
                    Some(val) => {
                        return Ok(val)
                    },
                    None => {
                        Err(Error::new(ErrorKind::InvalidInput, "Key not found"))
                    },
                }
            },
            Err(e) => {
                return Err(Error::new(ErrorKind::InvalidInput, e))
            },
        }
    }
    
    fn put_multiple_blocks(&self, blocks: &Vec<(Vec<u8> /* block_ser */, u64 /* batch starting sequence */)>) -> Result<(), Error> {
        let mut write_batch = WriteBatchWithTransaction::<false>::default();
        for (val, key) in blocks {
            write_batch.put(key, val);
        }
        let res = self.db.write_without_wal(write_batch);
        match res {
            Ok(_) => {
                return Ok(())
            },
            Err(e) => {
                return Err(Error::new(ErrorKind::BrokenPipe, e))
            },
        }
    }
}

