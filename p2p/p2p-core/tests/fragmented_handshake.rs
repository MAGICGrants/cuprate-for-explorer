//! This file contains a test for a handshake with monerod but uses fragmented messages.

#![expect(unused_crate_dependencies, reason = "external test module")]

use std::{
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::{Stream, StreamExt};
use tokio::{
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    time::timeout,
};
use tokio_util::{
    bytes::BytesMut,
    codec::{Encoder, FramedRead, FramedWrite},
};
use tower::{Service, ServiceExt};

use cuprate_helper::network::Network;
use cuprate_test_utils::monerod::monerod;
use cuprate_wire::{
    common::PeerSupportFlags,
    levin::{message::make_fragmented_messages, LevinMessage, Protocol},
    BasicNodeData, Message, MoneroWireCodec,
};

use cuprate_p2p_core::{
    client::{
        handshaker::HandshakerBuilder, ConnectRequest, Connector, DoHandshakeRequest,
        InternalPeerID,
    },
    transports::TcpServerConfig,
    ConnectionDirection, NetworkZone, Transport,
};

/// A network zone equal to clear net where every message sent is turned into a fragmented message.
/// Does not support sending fragmented or dummy messages manually.
#[derive(Clone, Copy)]
pub enum FragNet {}

#[async_trait::async_trait]
impl NetworkZone for FragNet {
    const NAME: &'static str = "FragNet";
    const CHECK_NODE_ID: bool = true;
    const BROADCAST_OWN_ADDR: bool = false;

    type Addr = SocketAddr;
}

#[derive(Clone)]
pub struct FragTcp;

#[async_trait::async_trait]
impl Transport<FragNet> for FragTcp {
    type Stream = FramedRead<OwnedReadHalf, MoneroWireCodec>;
    type Sink = FramedWrite<OwnedWriteHalf, FragmentCodec>;
    type Listener = InBoundStream;

    type ClientConfig = Option<()>;
    type ServerConfig = TcpServerConfig;

    async fn connect_to_peer(
        addr: <FragNet as NetworkZone>::Addr,
        _config: &Self::ClientConfig,
    ) -> Result<(Self::Stream, Self::Sink), std::io::Error> {
        let (read, write) = TcpStream::connect(addr).await?.into_split();
        Ok((
            FramedRead::new(read, MoneroWireCodec::default()),
            FramedWrite::new(write, FragmentCodec::default()),
        ))
    }

    async fn incoming_connection_listener(
        config: Self::ServerConfig,
    ) -> Result<Self::Listener, std::io::Error> {
        let listener = TcpListener::bind(SocketAddr::new(
            config
                .ipv4
                .expect("During test, we assume an IPv4 to be specified for the listener.")
                .into(),
            config.port,
        ))
        .await?;
        Ok(InBoundStream { listener })
    }
}

pub struct InBoundStream {
    listener: TcpListener,
}

impl Stream for InBoundStream {
    type Item = Result<
        (
            Option<SocketAddr>,
            FramedRead<OwnedReadHalf, MoneroWireCodec>,
            FramedWrite<OwnedWriteHalf, FragmentCodec>,
        ),
        std::io::Error,
    >;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.listener
            .poll_accept(cx)
            .map_ok(|(stream, addr)| {
                let (read, write) = stream.into_split();
                (
                    Some(addr),
                    FramedRead::new(read, MoneroWireCodec::default()),
                    FramedWrite::new(write, FragmentCodec::default()),
                )
            })
            .map(Some)
    }
}

#[derive(Default)]
pub struct FragmentCodec(MoneroWireCodec);

impl Encoder<LevinMessage<Message>> for FragmentCodec {
    type Error = <MoneroWireCodec as Encoder<LevinMessage<Message>>>::Error;

    fn encode(
        &mut self,
        item: LevinMessage<Message>,
        dst: &mut BytesMut,
    ) -> Result<(), Self::Error> {
        match item {
            LevinMessage::Body(body) => {
                // 66 is the minimum fragment size.
                let fragments = make_fragmented_messages(&Protocol::default(), 66, body).unwrap();

                for frag in fragments {
                    self.0.encode(frag.into(), dst)?;
                }
            }
            _ => unreachable!("Handshakes should only send bucket bodys"),
        }
        Ok(())
    }
}

#[tokio::test]
async fn fragmented_handshake_cuprate_to_monerod() {
    let monerod = monerod(["--fixed-difficulty=1", "--out-peers=0"]).await;

    let our_basic_node_data = BasicNodeData {
        my_port: 0,
        network_id: Network::Mainnet.network_id(),
        peer_id: 87980,
        support_flags: PeerSupportFlags::from(1_u32),
        rpc_port: 0,
        rpc_credits_per_hash: 0,
    };

    let handshaker = HandshakerBuilder::<FragNet, FragTcp>::new(our_basic_node_data, None).build();

    let mut connector = Connector::new(handshaker);

    connector
        .ready()
        .await
        .unwrap()
        .call(ConnectRequest {
            addr: monerod.p2p_addr(),
            permit: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn fragmented_handshake_monerod_to_cuprate() {
    let our_basic_node_data = BasicNodeData {
        my_port: 18081,
        network_id: Network::Mainnet.network_id(),
        peer_id: 87980,
        support_flags: PeerSupportFlags::from(1_u32),
        rpc_port: 0,
        rpc_credits_per_hash: 0,
    };

    let mut handshaker =
        HandshakerBuilder::<FragNet, FragTcp>::new(our_basic_node_data, None).build();

    let mut server_cfg = TcpServerConfig::default();
    server_cfg.ipv4 = Some("127.0.0.1".parse().unwrap());

    let mut listener = <FragTcp as Transport<FragNet>>::incoming_connection_listener(server_cfg)
        .await
        .unwrap();

    let _monerod = monerod(["--add-exclusive-node=127.0.0.1:18081"]).await;

    // Put a timeout on this just in case monerod doesn't make the connection to us.
    let next_connection_fut = timeout(Duration::from_secs(30), listener.next());

    if let Some(Ok((addr, stream, sink))) = next_connection_fut.await.unwrap() {
        handshaker
            .ready()
            .await
            .unwrap()
            .call(DoHandshakeRequest {
                addr: InternalPeerID::KnownAddr(addr.unwrap()), // This is clear net all addresses are known.
                peer_stream: stream,
                peer_sink: sink,
                direction: ConnectionDirection::Inbound,
                permit: None,
            })
            .await
            .unwrap();
    } else {
        panic!("Failed to receive connection from monerod.");
    };
}
