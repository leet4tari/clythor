//  Copyright 2024. The Tari Project
//
//  Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
//  following conditions are met:
//
//  1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
//  disclaimer.
//
//  2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
//  following disclaimer in the documentation and/or other materials provided with the distribution.
//
//  3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
//  products derived from this software without specific prior written permission.
//
//  THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
//  INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//  DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//  SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
//  SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
//  WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
//  USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use std::{
    convert::TryFrom,
    sync::{atomic::AtomicBool, Arc},
    thread,
    time::{Duration, Instant},
};
use dialoguer::Input as InputPrompt;
use log::{debug, error, info};
use minotari_app_utilities::parse_miner_input::process_quit;
use randomx_rs::RandomXFlag;
use reqwest::Client as ReqwestClient;
use tari_core::proof_of_work::{
    randomx_factory::{RandomXFactory, RandomXVMInstance},
    Difficulty,
};
use tari_shutdown::Shutdown;

use crate::{cli::Cli, error::{ConfigError, Error, MiningError, MiningError::TokioRuntime}, http, json_rpc::{get_block_template::get_block_template, submit_block::submit_block}, shared_dataset::SharedDataset, stats_store::StatsStore};
use crate::http::server::HttpServer;

pub const LOG_TARGET: &str = "clythor::main";

pub async fn start_miner(cli: Cli) -> Result<(), Error> {
    let node_address = monero_base_node_address(&cli)?;
    let monero_wallet_address = monero_wallet_address(&cli)?;
    let num_threads = cli.num_mining_threads.unwrap_or(num_cpus::get());

    let mut shutdown = Shutdown::new();
    let client = ReqwestClient::new();

    debug!(target: LOG_TARGET, "Starting new mining cycle");

    let flags = RandomXFlag::get_recommended_flags() | RandomXFlag::FLAG_FULL_MEM;
    let randomx_factory = RandomXFactory::new_with_flags(num_threads, flags);
    let shared_dataset = Arc::new(SharedDataset::default());
    let stats_store = Arc::new(StatsStore::new(num_threads));

    // http server
    let http_server_config = http::config::Config::new(cli.http_port.unwrap_or(18000u16));
    let http_server = HttpServer::new(shutdown.to_signal(), http_server_config, stats_store.clone());
    tokio::spawn(async move {
        if let Err(error) = http_server.start().await {
            error!("Failed to start HTTP server: {error:?}");
        }
    });

    info!(target: LOG_TARGET, "Starting {} threads", num_threads);

    thread::scope(|s| {
        for thread_index in 0..num_threads {
            let client = &client;
            let node_address = &node_address;
            let monero_wallet_address = &monero_wallet_address;
            let randomx_factory = &randomx_factory;
            let dataset = shared_dataset.clone();
            let stats = stats_store.clone();
            let cli = &cli;

            s.spawn(move || {
                thread_work(
                    num_threads,
                    thread_index,
                    client,
                    node_address,
                    monero_wallet_address,
                    randomx_factory,
                    dataset,
                    stats,
                    cli,
                )
            });
        }
    });

    shutdown.trigger();

    Ok(())
}

fn thread_work<'a>(
    num_threads: usize,
    thread_number: usize,
    client: &ReqwestClient,
    node_address: &'a str,
    monero_wallet_address: &'a str,
    randomx_factory: &RandomXFactory,
    shared_dataset: Arc<SharedDataset>,
    stats_store: Arc<StatsStore>,
    cli: &Cli,
) -> Result<(), MiningError> {
    let runtime = tokio::runtime::Runtime::new().map_err(|e| TokioRuntime(e.to_string()))?;
    let flags = randomx_factory.get_flags()?;

    let stop_flag = Arc::new(AtomicBool::new(false));

    runtime.spawn(check_template(
        client.clone(),
        node_address.to_string(),
        monero_wallet_address.to_string(),
        cli.template_refresh_interval_ms.unwrap_or(15000),
        stop_flag.clone(),
    ));

    // dataset control loop
    loop {
        let block_template = runtime.block_on(get_block_template(client, node_address, monero_wallet_address))?;
        let seed_hash = block_template.seed_hash;
        let vm_key = hex::decode(&seed_hash)?
            .clone()
            .into_iter()
            .chain(thread_number.to_le_bytes())
            .collect::<Vec<u8>>(); // RandomXFactory uses the key for caching the VM, and it should be unique, but also the key for the cache and
                                   // dataset can be shared
        let (dataset, cache) = shared_dataset.fetch_or_create_dataset(seed_hash.clone(), flags, thread_number)?;
        let vm = randomx_factory.create(&vm_key, Some(cache), Some(dataset))?;

        // block template loop
        'template: loop {
            // Fetch the block template again because dataset initialization takes a minute and the template could
            // change in that time.
            let block_template = runtime.block_on(get_block_template(client, node_address, monero_wallet_address))?;

            if seed_hash != block_template.seed_hash {
                info!(target: LOG_TARGET, "Thread {} detected seed hash change. Reinitializing dataset", thread_number);
                break 'template;
            }

            let mut blockhashing_bytes = hex::decode(block_template.blockhashing_blob.clone())?;

            let mut nonce = thread_number;
            let mut stats_last_check_time = Instant::now();
            let mut max_difficulty_reached = 0;

            debug!(target: LOG_TARGET, "Thread {} ⛏️ Mining now", thread_number);
            stats_store.start();
            let mut template_refresh_time = Instant::now();
            let cycle_start = Instant::now();
            let mut hash_count = 0u64;
            'mining: loop {
                if template_refresh_time.elapsed().as_millis() >= 500 {
                    template_refresh_time = Instant::now();

                    if stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                        info!(
                            target: LOG_TARGET,
                            "Thead {} detected template change. Restarting mining cycle",
                            thread_number
                        );
                        stop_flag.store(false, std::sync::atomic::Ordering::Relaxed);
                        break 'mining;
                    }
                }

                let (difficulty, hash) = mining_cycle(&mut blockhashing_bytes, u32::try_from(nonce)?, vm.clone())?;
                hash_count += 1;

                if difficulty.as_u64() > max_difficulty_reached {
                    max_difficulty_reached = difficulty.as_u64();
                }

                let elapsed_since_last_check = Instant::now().duration_since(stats_last_check_time);
                // Spread out the updates by a few MS to reduce the chances of multiple threads trying to update
                // the AtomicU64 at the same time.
                let check_time =
                    Duration::from_secs(5) + Duration::from_millis((num_threads * 100 / (thread_number+1)) as u64);
                if elapsed_since_last_check >= check_time {
                    info!(target: LOG_TARGET, "{}", stats_store.pretty_print(thread_number, nonce, cycle_start.elapsed().as_secs(), max_difficulty_reached, block_template.difficulty));
                    stats_last_check_time = Instant::now();
                    stats_store.inc_hashed_count_by(hash_count);
                    hash_count = 0;
                }

                if difficulty.as_u64() >= block_template.difficulty {
                    let mut block_template_bytes = hex::decode(&block_template.blocktemplate_blob)?;
                    block_template_bytes[0..42].copy_from_slice(&hash[0..42]);

                    let block_hex = hex::encode(block_template_bytes.clone());

                    match runtime
                        .block_on(submit_block(client, node_address, block_hex))
                        .map_err(MiningError::Request)
                    {
                        Ok(_) => {
                            debug!(target: LOG_TARGET, "Thread {} submitted block with hash: {} with difficulty: {} successfully", thread_number, hex::encode(&hash[0..42]), difficulty);
                            info!(target: LOG_TARGET, "Thread {} found a block! 🎉", thread_number);
                        },
                        Err(e) => {
                            debug!(target: LOG_TARGET, "Thread {} failed to submit block: {}", thread_number, e);
                        },
                    }

                    break 'mining;
                }
                nonce += num_threads;
            }
        }
    }
}

fn mining_cycle(
    blockhashing_bytes: &mut Vec<u8>,
    nonce: u32,
    vm: RandomXVMInstance,
) -> Result<(Difficulty, &Vec<u8>), MiningError> {
    let nonce_position = 38;
    blockhashing_bytes[nonce_position..nonce_position + 4].copy_from_slice(&nonce.to_le_bytes());

    // We could but we won't
    // let timestamp_position = 8;
    // let timestamp_bytes: [u8; 4] = u32::try_from(EpochTime::now().as_u64())?.to_le_bytes();
    // blockhashing_bytes[timestamp_position..timestamp_position + 4].copy_from_slice(&timestamp_bytes);

    let hash = vm.calculate_hash(blockhashing_bytes)?;
    // Check last byte of hash and see if it's over difficulty
    let difficulty = Difficulty::little_endian_difficulty(&hash)?;

    Ok((difficulty, blockhashing_bytes))
}

async fn check_template(
    client: ReqwestClient,
    node_address: String,
    monero_wallet_address: String,
    template_refresh_interval_ms: u64,
    stop_flag: Arc<AtomicBool>,
) -> Result<(), Error> {
    let mut block_template = get_block_template(&client, &node_address, &monero_wallet_address).await?;

    loop {
        let new_block_template = get_block_template(&client, &node_address, &monero_wallet_address).await?;

        if block_template.blocktemplate_blob != new_block_template.blocktemplate_blob {
            stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            block_template = new_block_template;
        }

        tokio::time::sleep(Duration::from_millis(template_refresh_interval_ms)).await;
    }
}

fn monero_base_node_address(cli: &Cli) -> Result<String, ConfigError> {
    let monero_base_node_address = cli
        .monero_base_node_address
        .as_ref()
        .cloned()
        .or_else(|| {
            if cli.non_interactive_mode {
                None
            } else {
                let base_node = InputPrompt::<String>::new()
                    .with_prompt("Please enter the 'monero-base-node-address' ('quit' or 'exit' to quit) ")
                    .interact()
                    .unwrap();
                process_quit(&base_node);
                Some(base_node.trim().to_string())
            }
        })
        .ok_or(ConfigError::MissingBaseNode)?;

    info!(target: LOG_TARGET, "Using Monero node address: {}", &monero_base_node_address);

    Ok(monero_base_node_address)
}

fn monero_wallet_address(cli: &Cli) -> Result<String, ConfigError> {
    let monero_wallet_address = cli
        .monero_wallet_address
        .as_ref()
        .cloned()
        .or_else(|| {
            if cli.non_interactive_mode {
                None
            } else {
                let address = InputPrompt::<String>::new()
                    .with_prompt("Please enter the 'monero-wallet-address' ('quit' or 'exit' to quit) ")
                    .interact()
                    .unwrap();
                process_quit(&address);
                Some(address.trim().to_string())
            }
        })
        .ok_or(ConfigError::MissingMoneroWalletAddress)?;

    info!(target: LOG_TARGET, "Mining to Monero wallet address: {}", &monero_wallet_address);

    Ok(monero_wallet_address)
}
