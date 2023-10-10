use completeio::net::{TcpListener, TcpStream, ToSockAddrs};

async fn test_impl(addr: impl ToSockAddrs) {
    let listener = TcpListener::bind(addr).unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = futures_channel::oneshot::channel();
    completeio::task::spawn(async move {
        let (socket, accepted_addr) = listener.accept().await.unwrap();

        assert!(tx.send((socket, accepted_addr)).is_ok());
    })
    .detach();
    let cli = TcpStream::connect(&addr).await.unwrap();
    let (srv, accepted_addr) = rx.await.unwrap();
    let client_addr = cli.local_addr().unwrap();
    assert_eq!(client_addr, srv.peer_addr().unwrap());
    assert_eq!(client_addr, accepted_addr);
}

macro_rules! test_accept {
    ($(($ident:ident, $target:expr),)*) => {
        $(
            #[test]
            fn $ident() {
                println!("Testing {}...", stringify!($ident));
                completeio::task::block_on(test_impl($target))
            }
        )*
    };
}

test_accept! {
    (ip_str, "127.0.0.1:0"),
    (host_str, "localhost:0"),
    (socket_addr, "127.0.0.1:0".parse::<std::net::SocketAddr>().unwrap()),
    (str_port_tuple, ("127.0.0.1", 0)),
    (ip_port_tuple, ("127.0.0.1".parse::<std::net::IpAddr>().unwrap(), 0)),
}
