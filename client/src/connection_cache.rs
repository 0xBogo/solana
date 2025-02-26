use {
    quinn::Endpoint,
    solana_connection_cache::{
        client_connection::ClientConnection as BlockingClientConnection,
        connection_cache::{
            ConnectionCache as BackendConnectionCache, NewConnectionConfig, ProtocolType,
        },
        nonblocking::client_connection::ClientConnection as NonblockingClientConnection,
    },
    solana_quic_client::{QuicConfig, QuicConnectionManager},
    solana_sdk::{pubkey::Pubkey, signature::Keypair},
    solana_streamer::streamer::StakedNodes,
    solana_udp_client::UdpConnectionManager,
    std::{
        error::Error,
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{Arc, RwLock},
    },
};

pub const DEFAULT_CONNECTION_POOL_SIZE: usize = 4;
pub const DEFAULT_CONNECTION_CACHE_USE_QUIC: bool = true;
pub const MAX_CONNECTIONS: usize = 1024;

/// A thin wrapper over connection-cache/ConnectionCache to ease
/// construction of the ConnectionCache for code dealing both with udp and quic.
/// For the scenario only using udp or quic, use connection-cache/ConnectionCache directly.
pub struct ConnectionCache {
    cache: BackendConnectionCache,
}

impl ConnectionCache {
    /// Create a quic connection_cache
    pub fn new(connection_pool_size: usize) -> Self {
        Self::new_with_client_options(connection_pool_size, None, None, None)
    }

    /// Create a quic conneciton_cache with more client options
    pub fn new_with_client_options(
        connection_pool_size: usize,
        client_endpoint: Option<Endpoint>,
        cert_info: Option<(&Keypair, IpAddr)>,
        stake_info: Option<(&Arc<RwLock<StakedNodes>>, &Pubkey)>,
    ) -> Self {
        // The minimum pool size is 1.
        let connection_pool_size = 1.max(connection_pool_size);
        let mut config = QuicConfig::new().unwrap();
        if let Some(client_endpoint) = client_endpoint {
            config.update_client_endpoint(client_endpoint);
        }
        if let Some(cert_info) = cert_info {
            config
                .update_client_certificate(cert_info.0, cert_info.1)
                .unwrap();
        }
        if let Some(stake_info) = stake_info {
            config.set_staked_nodes(stake_info.0, stake_info.1);
        }
        let connection_manager =
            Box::new(QuicConnectionManager::new_with_connection_config(config));
        let cache = BackendConnectionCache::new(connection_manager, connection_pool_size).unwrap();
        Self { cache }
    }

    #[deprecated(
        since = "1.15.0",
        note = "This method does not do anything. Please use `new_with_client_options` instead to set the client certificate."
    )]
    pub fn update_client_certificate(
        &mut self,
        _keypair: &Keypair,
        _ipaddr: IpAddr,
    ) -> Result<(), Box<dyn Error>> {
        Ok(())
    }

    #[deprecated(
        since = "1.15.0",
        note = "This method does not do anything. Please use `new_with_client_options` instead to set staked nodes information."
    )]
    pub fn set_staked_nodes(
        &mut self,
        _staked_nodes: &Arc<RwLock<StakedNodes>>,
        _client_pubkey: &Pubkey,
    ) {
    }

    pub fn with_udp(connection_pool_size: usize) -> Self {
        // The minimum pool size is 1.
        let connection_pool_size = 1.max(connection_pool_size);
        let connection_manager = Box::<UdpConnectionManager>::default();
        let cache = BackendConnectionCache::new(connection_manager, connection_pool_size).unwrap();
        Self { cache }
    }

    pub fn use_quic(&self) -> bool {
        matches!(self.cache.get_protocol_type(), ProtocolType::QUIC)
    }

    pub fn get_connection(&self, addr: &SocketAddr) -> Arc<dyn BlockingClientConnection> {
        self.cache.get_connection(addr)
    }

    pub fn get_nonblocking_connection(
        &self,
        addr: &SocketAddr,
    ) -> Arc<dyn NonblockingClientConnection> {
        self.cache.get_nonblocking_connection(addr)
    }
}

impl Default for ConnectionCache {
    fn default() -> Self {
        if DEFAULT_CONNECTION_CACHE_USE_QUIC {
            let cert_info = (&Keypair::new(), IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
            ConnectionCache::new_with_client_options(
                DEFAULT_CONNECTION_POOL_SIZE,
                None,
                Some(cert_info),
                None,
            )
        } else {
            ConnectionCache::with_udp(DEFAULT_CONNECTION_POOL_SIZE)
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::connection_cache::ConnectionCache,
        crossbeam_channel::unbounded,
        solana_sdk::{quic::QUIC_PORT_OFFSET, signature::Keypair},
        solana_streamer::{
            nonblocking::quic::DEFAULT_WAIT_FOR_CHUNK_TIMEOUT_MS, quic::StreamStats,
            streamer::StakedNodes,
        },
        std::{
            net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
            sync::{
                atomic::{AtomicBool, Ordering},
                Arc, RwLock,
            },
        },
    };

    fn server_args() -> (
        UdpSocket,
        Arc<AtomicBool>,
        Keypair,
        IpAddr,
        Arc<StreamStats>,
    ) {
        (
            UdpSocket::bind("127.0.0.1:0").unwrap(),
            Arc::new(AtomicBool::new(false)),
            Keypair::new(),
            "127.0.0.1".parse().unwrap(),
            Arc::new(StreamStats::default()),
        )
    }

    #[test]
    fn test_connection_with_specified_client_endpoint() {
        let port = u16::MAX - QUIC_PORT_OFFSET + 1;
        assert!(port.checked_add(QUIC_PORT_OFFSET).is_none());

        // Start a response receiver:
        let (
            response_recv_socket,
            response_recv_exit,
            keypair2,
            response_recv_ip,
            response_recv_stats,
        ) = server_args();
        let (sender2, _receiver2) = unbounded();

        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));

        let (response_recv_endpoint, response_recv_thread) = solana_streamer::quic::spawn_server(
            response_recv_socket,
            &keypair2,
            response_recv_ip,
            sender2,
            response_recv_exit.clone(),
            1,
            staked_nodes,
            10,
            10,
            response_recv_stats,
            DEFAULT_WAIT_FOR_CHUNK_TIMEOUT_MS,
        )
        .unwrap();

        let connection_cache =
            ConnectionCache::new_with_client_options(1, Some(response_recv_endpoint), None, None);

        // server port 1:
        let port1 = 9001;
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port1);
        let conn = connection_cache.get_connection(&addr);
        assert_eq!(conn.server_addr().port(), port1 + QUIC_PORT_OFFSET);

        // server port 2:
        let port2 = 9002;
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port2);
        let conn = connection_cache.get_connection(&addr);
        assert_eq!(conn.server_addr().port(), port2 + QUIC_PORT_OFFSET);

        response_recv_exit.store(true, Ordering::Relaxed);
        response_recv_thread.join().unwrap();
    }
}
