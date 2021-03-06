// 两个实例，某个具体协议的 inbound 就一定对应此具体协议
// 所以 local-proxy inbound 只需要socks就行，
// 其他协议的 inbound，通过 local-proxy#outbound => remote-proxy-server#inbound 测试

use std::{net::SocketAddr, str::FromStr, time::Duration, convert::TryFrom};

use futures::{future::BoxFuture, FutureExt};
use log::debug;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    runtime::Builder,
};
use tunnel::{
    proxy::{Address, Session, addr_to_tuple},
    start,
};
pub async fn tcp_echo_server(addr: SocketAddr) {
    let listener = TcpListener::bind(addr).await.unwrap();
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(async {
                    let (mut read_half, mut write_half) = stream.into_split();
                    tokio::io::copy(&mut read_half, &mut write_half).await
                });
            }
            Err(err) => {
                eprint!("error occurred at listener#accept {}", err)
            }
        }
    }
}
pub async fn udp_echo_server(bind_addr: SocketAddr) {
    let socket = UdpSocket::bind(bind_addr).await.unwrap();
    let mut buf = Vec::new();
    loop {
        let (n, remote_addr) = match socket.recv_from(&mut buf).await {
            Ok(x) => x,
            Err(err) => {
                eprintln!("{}", err);
                continue;
            }
        };
        match socket.send_to(&buf[..n], remote_addr).await {
            Err(err) => {
                eprintln!("{}", err);
            }
            _ => {}
        }
    }
}

pub fn run_two_of_echo_server(bind_addr: SocketAddr) -> Vec<BoxFuture<'static, ()>> {
    let mut tasks = Vec::new();
    let f = tcp_echo_server(bind_addr.clone()).boxed();
    tasks.push(f);
    let f = udp_echo_server(bind_addr.clone()).boxed();
    tasks.push(f);
    tasks
}

// should be called on the tokio runtime context
pub fn start_tunnel(
    configs: Vec<tunnel::config::Config>,
    echo_server_listening_at: &str,
    socks_server_listening_at: &str,
) {
    let buf = "helloworld".as_bytes();
    let rt = Builder::new_current_thread().enable_all().build().unwrap();
    let mut abort_handlers = Vec::new();

    for config in configs {
        let (shutdown_future, shutdown_handler) =
            futures::future::abortable(futures::future::pending::<bool>());
        abort_handlers.push(shutdown_handler);
        rt.spawn_blocking(|| {
            let handler = async {
                shutdown_future.await;
            }
            .boxed();
            start(config, handler).unwrap();
        });
    }

    let mut tasks = Vec::new();
    // echo server is the real remote server that we want to connected.
    let mut echo_futures =
        run_two_of_echo_server(SocketAddr::from_str(echo_server_listening_at).unwrap());
    tasks.append(&mut echo_futures);

    let (abort_future, abort_handler) =
        futures::future::abortable(futures::future::join_all(tasks));
    let test_future = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        send_data_socks5_tcp(socks_server_listening_at, echo_server_listening_at, &buf)
            .await
            .unwrap();
        // call abort handler after test completed
        abort_handler.abort();
    };
    rt.block_on(futures::future::join(abort_future, test_future));

    rt.shutdown_background();
    // stop all instance
    for handler in abort_handlers {
        handler.abort();
    }
}

async fn send_data_socks5_tcp(
    proxy_server: &str,
    remote_server: &str,
    buf: &[u8],
) -> anyhow::Result<()> {
    let addr = proxy_server.parse::<SocketAddr>().unwrap();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    debug!("{} {}", proxy_server, remote_server);
    let session = Session {
        destination: Address::try_from(addr_to_tuple(remote_server)).unwrap(),
        local_peer: stream.local_addr().unwrap(),
        network: tunnel::proxy::Network::TCP,
        peer_address: stream.peer_addr().unwrap()
    };
    tunnel::proxy::socks::handshake_as_client(&mut stream, &session).await?;
    stream.write_all(&buf).await?;
    let mut received = vec![0; buf.len()];
    stream.read_exact(&mut received).await.unwrap();
    assert_eq!(buf, received);
    Ok(())
}
