// Copyright (c) Shubham Mishra. All rights reserved.
// Licensed under the MIT License.

use log::{debug, error, info};
use pft::config::{self, Config};
use pft::consensus;
use pft::consensus::batch_proposal::TxWithAckChanTag;
use pft::utils::channel::{make_channel, Receiver, Sender};
use tokio::{runtime, signal};
use std::io::Write;
use std::{env, fs, io, path, sync::{atomic::AtomicUsize, Arc, Mutex}};
use pft::consensus::engines::kvs::KVSAppEngine;

mod frontend;
mod payloads;

// #[global_allocator]
// static ALLOC: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;

/// Fetch json config file from command line path.
/// Panic if not found or parsed properly.

struct Arguments {
    config: Config,
    send_threshold: i64,
}

fn process_args() -> Arguments {
    macro_rules! usage_str {
        () => {
            "\x1b[31;1mUsage: {} path/to/config.json\x1b[0m"
        };
    }

    let args: Vec<_> = env::args().collect();

    if args.len() != 3 {
        panic!(usage_str!(), args[0]);
    }

    let cfg_path = path::Path::new(args[1].as_str());
    if !cfg_path.exists() {
        panic!(usage_str!(), args[0]);
    }

    let cfg_contents = fs::read_to_string(cfg_path).expect("Invalid file path");

    let send_threshold: i64 = args[2].parse().unwrap();

    Arguments{
        config: Config::deserialize(&cfg_contents),
        send_threshold: send_threshold,
    }
}

#[allow(unused_assignments)]
fn get_feature_set() -> (&'static str, &'static str) {
    ("pirateship", "kms")
}

async fn test_run() {
    let v = &["apples", "cake", "coffee"];

    for text in v {
        println!("I like {}.", text);
    }
}


async fn run_main(config: Config, batch_proposer_tx: Sender<TxWithAckChanTag>, batch_proposer_rx: Receiver<TxWithAckChanTag>) -> io::Result<()> {    
    let mut node = consensus::ConsensusNode::<KVSAppEngine>::mew(config.clone(), batch_proposer_tx, batch_proposer_rx);

    // let mut handles = consensus::ConsensusNode::run(node);
    let mut handles = node.run().await;

    match signal::ctrl_c().await {
        Ok(_) => {
            info!("Received SIGINT. Shutting down.");
            handles.abort_all();
        },
        Err(e) => {
            error!("Signal: {:?}", e);
        }
    }

    while let Some(res) = handles.join_next().await {
        info!("Task completed with {:?}", res);
    }
    Ok(())
}

const NUM_THREADS: usize = 32;

fn main() {
    log4rs::init_config(config::default_log4rs_config()).unwrap();

    let args = process_args();
    let cfg = args.config;
    let send_threshold = args.send_threshold;

    let (protocol, app) = get_feature_set();
    info!("Protocol: {}, App: {}", protocol, app);


    let core_ids = 
        Arc::new(Mutex::new(Box::pin(core_affinity::get_core_ids().unwrap())));

    let start_idx = cfg.consensus_config.node_list.iter().position(|r| r.eq(&cfg.net_config.name)).unwrap();

    let (actix_threads, consensus_threads) = {
        let _num_cores = core_ids.lock().unwrap().len();
        if _num_cores == 1 {
            // This will have a terrible performance, but it will work!
            (1, 1)
        } else if _num_cores > 4 {
            (4, _num_cores - 4)
        } else {
            (1, _num_cores - 1)
        }
    };
    // let num_threads = 4;

    let (batch_proposer_tx, batch_proposer_rx) = make_channel(cfg.rpc_config.channel_depth as usize);

    let start_idx = start_idx * consensus_threads;
    
    let i = Box::pin(AtomicUsize::new(0));
    let runtime = runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(consensus_threads)
        .on_thread_start(move || {
            let _cids = core_ids.clone();
            let lcores = _cids.lock().unwrap();
            let id = (start_idx + i.fetch_add(1, std::sync::atomic::Ordering::SeqCst)) % lcores.len();
            let res = core_affinity::set_for_current(lcores[id]);
            
            if res {
                debug!("Thread pinned to core {:?}", id);
            }else{
                debug!("Thread pinning to core {:?} failed", id);
            }

            std::io::stdout().flush()
                .unwrap();
        })
        .build()
        .unwrap();
    
    //run front end server

    let _ = runtime.spawn(run_main(cfg.clone(), batch_proposer_tx.clone(), batch_proposer_rx));
    
    let frontend_runtime = runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(actix_threads) 
        .build()
        .unwrap();
    match frontend_runtime.block_on(frontend::run_actix_server(cfg, batch_proposer_tx, actix_threads, send_threshold)) {
        Ok(_) => println!("Frontend server ran successfully."),
        Err(e) => eprintln!("Frontend server error: {:?}", e),
    };

}
