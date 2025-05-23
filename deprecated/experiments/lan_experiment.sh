#!/bin/bash

# Copyright (c) Shubham Mishra. All rights reserved.
# Licensed under the MIT License.

set -o xtrace

# ALL_CLIENTS="-c 1 -c 10 -c 100 -c 500 -c 700 -c 1000 -c 1200 -c 1500 -c 2000"
# ALL_CLIENTS="-c 100 -c 200 -c 300 -c 500 -c 700 -c 900 -c 1000 -c 1200 -c 1500 -c 1800 -c 2000 -c 2500"
ALL_CLIENTS="-c 2000"
# ALL_CLIENTS="-c 10 -c 20 -c 30 -c 40"
# ALL_CLIENTS="-c 50 -c 60 -c 70 -c 80"
# ALL_CLIENTS="-c 60 -c 70 -c 90 -c 100 -c 1200"
# ALL_CLIENTS="-c 900 -c 1500 -c 2500 -c 5000"
# ALL_CLIENTS="-c 200 -c 300 -c 500 -c 1000 -c 1500"
# ALL_CLIENTS="-c 1800 -c 2000 -c 2500"
# ALL_CLIENTS="-c 1700 -c 2000 -c 2500 -c 3000"
# ALL_CLIENTS="-c 10 -c 500 -c 1200 -c 1500"
# ALL_CLIENTS="-c 1200 -c 1500"
# ALL_CLIENTS="-c 20 -c 500 -c 1500"
# ALL_CLIENTS="-c 3000"
# ALL_CLIENTS="-c 1200 -c 1500"
# ALL_CLIENTS="-c 200 -c 1000"

start_time=$(date -Ins)
# start_time='2024-08-07T10:39:54.859389+00:00'
RUN_CMD="python3 scripts/run_remote_client_sweep.py -nt /tmp/local_template.json -ct scripts/local_client_template.json -ips ../nodelist.txt -i ../cluster_key.pem -r 1 -s 60 -up 10 -down 10 $ALL_CLIENTS"


jq '.consensus_config.max_backlog_batch_size = 1000 | .consensus_config.quorum_diversity_k = 3 | .consensus_config.signature_max_delay_blocks = 50 | .consensus_config.liveness_u = 2 | .consensus_config.view_timeout_ms = 5000' scripts/local_template.json > /tmp/local_template.json

# Run pirateship
# make
# $RUN_CMD

# # Run chained_pbft
# make chained_pbft_logger
# $RUN_CMD

# # Run signed_raft
# make signed_raft_logger
# $RUN_CMD --max_nodes 5


# Run lucky_raft
make lucky_raft_logger
$RUN_CMD


end_time=$(date -Ins)

# Plot together
python3 scripts/plot_time_range_client_sweep.py \
    --path logs --end $end_time --start $start_time \
    -r 1 -c 1 -l node1 -up 30 -down 30 -o plot.png \
    --legend "pbft+onlybyz"
    # --legend "signed_raft"
    # --legend "pirateship+byz" \
