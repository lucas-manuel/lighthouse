#![cfg(test)]
use libp2p::gossipsub::GossipsubConfigBuilder;
use lighthouse_network::Enr;
use lighthouse_network::EnrExt;
use lighthouse_network::Multiaddr;
use lighthouse_network::Service as LibP2PService;
use lighthouse_network::{Libp2pEvent, NetworkConfig};
use slog::{debug, error, o, Drain};
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;
use tokio::runtime::Runtime;
use types::{ChainSpec, EnrForkId, EthSpec, ForkContext, Hash256, MinimalEthSpec};
use unused_port::unused_tcp_port;

#[allow(clippy::type_complexity)]
#[allow(unused)]
pub mod behaviour;
#[allow(clippy::type_complexity)]
#[allow(unused)]
pub mod swarm;

type E = MinimalEthSpec;
type ReqId = usize;

use tempfile::Builder as TempBuilder;

/// Returns a dummy fork context
pub fn fork_context() -> ForkContext {
    let mut chain_spec = E::default_spec();
    // Set fork_epoch to `Some` to ensure that the `ForkContext` object
    // includes altair in the list of forks
    chain_spec.altair_fork_epoch = Some(types::Epoch::new(42));
    chain_spec.bellatrix_fork_epoch = Some(types::Epoch::new(84));
    ForkContext::new::<E>(types::Slot::new(0), Hash256::zero(), &chain_spec)
}

pub struct Libp2pInstance(LibP2PService<ReqId, E>, exit_future::Signal);

impl std::ops::Deref for Libp2pInstance {
    type Target = LibP2PService<ReqId, E>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Libp2pInstance {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[allow(unused)]
pub fn build_log(level: slog::Level, enabled: bool) -> slog::Logger {
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator).build().fuse();
    let drain = slog_async::Async::new(drain).build().fuse();

    if enabled {
        slog::Logger::root(drain.filter_level(level).fuse(), o!())
    } else {
        slog::Logger::root(drain.filter(|_| false).fuse(), o!())
    }
}

pub fn build_config(port: u16, mut boot_nodes: Vec<Enr>) -> NetworkConfig {
    let mut config = NetworkConfig::default();
    let path = TempBuilder::new()
        .prefix(&format!("libp2p_test{}", port))
        .tempdir()
        .unwrap();

    config.libp2p_port = port; // tcp port
    config.discovery_port = port; // udp port
    config.enr_tcp_port = Some(port);
    config.enr_udp_port = Some(port);
    config.enr_address = Some("127.0.0.1".parse().unwrap());
    config.boot_nodes_enr.append(&mut boot_nodes);
    config.network_dir = path.into_path();
    // Reduce gossipsub heartbeat parameters
    config.gs_config = GossipsubConfigBuilder::from(config.gs_config)
        .heartbeat_initial_delay(Duration::from_millis(500))
        .heartbeat_interval(Duration::from_millis(500))
        .build()
        .unwrap();
    config
}

pub async fn build_libp2p_instance(
    rt: Weak<Runtime>,
    boot_nodes: Vec<Enr>,
    log: slog::Logger,
) -> Libp2pInstance {
    let port = unused_tcp_port().unwrap();
    let config = build_config(port, boot_nodes);
    // launch libp2p service

    let (signal, exit) = exit_future::signal();
    let (shutdown_tx, _) = futures::channel::mpsc::channel(1);
    let executor = task_executor::TaskExecutor::new(rt, exit, log.clone(), shutdown_tx);
    let libp2p_context = lighthouse_network::Context {
        config: &config,
        enr_fork_id: EnrForkId::default(),
        fork_context: Arc::new(fork_context()),
        chain_spec: &ChainSpec::minimal(),
        gossipsub_registry: None,
    };
    Libp2pInstance(
        LibP2PService::new(executor, libp2p_context, &log)
            .await
            .expect("should build libp2p instance")
            .1,
        signal,
    )
}

#[allow(dead_code)]
pub fn get_enr(node: &LibP2PService<ReqId, E>) -> Enr {
    node.swarm.behaviour().local_enr()
}

// Returns `n` libp2p peers in fully connected topology.
#[allow(dead_code)]
pub async fn build_full_mesh(
    rt: Weak<Runtime>,
    log: slog::Logger,
    n: usize,
) -> Vec<Libp2pInstance> {
    let mut nodes = Vec::with_capacity(n);
    for _ in 0..n {
        nodes.push(build_libp2p_instance(rt.clone(), vec![], log.clone()).await);
    }
    let multiaddrs: Vec<Multiaddr> = nodes
        .iter()
        .map(|x| get_enr(x).multiaddr()[1].clone())
        .collect();

    for (i, node) in nodes.iter_mut().enumerate().take(n) {
        for (j, multiaddr) in multiaddrs.iter().enumerate().skip(i) {
            if i != j {
                match libp2p::Swarm::dial(&mut node.swarm, multiaddr.clone()) {
                    Ok(()) => debug!(log, "Connected"),
                    Err(_) => error!(log, "Failed to connect"),
                };
            }
        }
    }
    nodes
}

// Constructs a pair of nodes with separate loggers. The sender dials the receiver.
// This returns a (sender, receiver) pair.
#[allow(dead_code)]
pub async fn build_node_pair(
    rt: Weak<Runtime>,
    log: &slog::Logger,
) -> (Libp2pInstance, Libp2pInstance) {
    let sender_log = log.new(o!("who" => "sender"));
    let receiver_log = log.new(o!("who" => "receiver"));

    let mut sender = build_libp2p_instance(rt.clone(), vec![], sender_log).await;
    let mut receiver = build_libp2p_instance(rt, vec![], receiver_log).await;

    let receiver_multiaddr = receiver.swarm.behaviour_mut().local_enr().multiaddr()[1].clone();

    // let the two nodes set up listeners
    let sender_fut = async {
        loop {
            if let Libp2pEvent::NewListenAddr(_) = sender.next_event().await {
                return;
            }
        }
    };
    let receiver_fut = async {
        loop {
            if let Libp2pEvent::NewListenAddr(_) = receiver.next_event().await {
                return;
            }
        }
    };

    let joined = futures::future::join(sender_fut, receiver_fut);

    // wait for either both nodes to listen or a timeout
    tokio::select! {
        _  = tokio::time::sleep(Duration::from_millis(500)) => {}
        _ = joined => {}
    }

    match libp2p::Swarm::dial(&mut sender.swarm, receiver_multiaddr.clone()) {
        Ok(()) => {
            debug!(log, "Sender dialed receiver"; "address" => format!("{:?}", receiver_multiaddr))
        }
        Err(_) => error!(log, "Dialing failed"),
    };
    (sender, receiver)
}

// Returns `n` peers in a linear topology
#[allow(dead_code)]
pub async fn build_linear(rt: Weak<Runtime>, log: slog::Logger, n: usize) -> Vec<Libp2pInstance> {
    let mut nodes = Vec::with_capacity(n);
    for _ in 0..n {
        nodes.push(build_libp2p_instance(rt.clone(), vec![], log.clone()).await);
    }

    let multiaddrs: Vec<Multiaddr> = nodes
        .iter()
        .map(|x| get_enr(x).multiaddr()[1].clone())
        .collect();
    for i in 0..n - 1 {
        match libp2p::Swarm::dial(&mut nodes[i].swarm, multiaddrs[i + 1].clone()) {
            Ok(()) => debug!(log, "Connected"),
            Err(_) => error!(log, "Failed to connect"),
        };
    }
    nodes
}
