use std::net::Ipv4Addr;

use completeio::net::{TcpListener, TcpStream};

fn main() {
    completeio::task::block_on(async {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();

        let (tx, (rx, _)) =
            futures_util::try_join!(TcpStream::connect(&addr), listener.accept()).unwrap();

        tx.send_all("Hello world!").await.0.unwrap();

        let buffer = Vec::with_capacity(12);
        let (n, buffer) = rx.recv_exact(buffer).await;
        assert_eq!(n.unwrap(), buffer.len());
        println!("{}", String::from_utf8(buffer).unwrap());
    });
}
