use criterion::{async_executor::AsyncExecutor, criterion_group, criterion_main, Criterion};

criterion_group!(net, tcp, udp);
criterion_main!(net);

struct CompleteIoRuntime;

impl AsyncExecutor for CompleteIoRuntime {
    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        completeio::task::block_on(future)
    }
}

fn tcp(c: &mut Criterion) {
    const PACKET_LEN: usize = 1048576;
    static PACKET: &[u8] = &[1u8; PACKET_LEN];

    let mut group = c.benchmark_group("tcp");

    group.bench_function("tokio", |b| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        b.to_async(&runtime).iter(|| async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let tx = tokio::net::TcpStream::connect(addr);
            let rx = listener.accept();
            let (mut tx, (mut rx, _)) = tokio::try_join!(tx, rx).unwrap();
            tx.write_all(PACKET).await.unwrap();
            let mut buffer = Vec::with_capacity(PACKET_LEN);
            while buffer.len() < PACKET_LEN {
                rx.read_buf(&mut buffer).await.unwrap();
            }
            buffer
        })
    });

    group.bench_function("completeio", |b| {
        b.to_async(CompleteIoRuntime).iter(|| async {
            let listener = completeio::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let tx = completeio::net::TcpStream::connect(addr);
            let rx = listener.accept();
            let (tx, (rx, _)) = futures_util::try_join!(tx, rx).unwrap();
            tx.send_all(PACKET).await.0.unwrap();
            let buffer = Vec::with_capacity(PACKET_LEN);
            let (recv, buffer) = rx.recv_exact(buffer).await;
            recv.unwrap();
            buffer
        })
    });

    group.finish();
}

fn udp(c: &mut Criterion) {
    const PACKET_LEN: usize = 1024;
    static PACKET: &[u8] = &[1u8; PACKET_LEN];

    let mut group = c.benchmark_group("udp");

    // The socket may be dropped by firewall when the number is too large.
    #[cfg(target_os = "linux")]
    group
        .sample_size(16)
        .measurement_time(std::time::Duration::from_millis(2))
        .warm_up_time(std::time::Duration::from_millis(2));

    group.bench_function("tokio", |b| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        b.to_async(&runtime).iter(|| async {
            let rx = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr_rx = rx.local_addr().unwrap();
            let tx = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr_tx = tx.local_addr().unwrap();

            rx.connect(addr_tx).await.unwrap();
            tx.connect(addr_rx).await.unwrap();

            {
                let mut pos = 0;
                while pos < PACKET_LEN {
                    let res = tx.send(&PACKET[pos..]).await;
                    pos += res.unwrap();
                }
            }
            {
                let mut buffer = vec![0; PACKET_LEN];
                let mut pos = 0;
                while pos < PACKET_LEN {
                    let res = rx.recv(&mut buffer[pos..]).await;
                    pos += res.unwrap();
                }
                buffer
            }
        })
    });

    group.bench_function("completeio", |b| {
        b.to_async(CompleteIoRuntime).iter(|| async {
            let rx = completeio::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let addr_rx = rx.local_addr().unwrap();
            let tx = completeio::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let addr_tx = tx.local_addr().unwrap();

            rx.connect(addr_tx).unwrap();
            tx.connect(addr_rx).unwrap();

            {
                let mut pos = 0;
                while pos < PACKET_LEN {
                    let (res, _) = tx.send(&PACKET[pos..]).await;
                    pos += res.unwrap();
                }
            }
            {
                let mut buffer = Vec::with_capacity(PACKET_LEN);
                let mut res;
                while buffer.len() < PACKET_LEN {
                    (res, buffer) = rx.recv(buffer).await;
                    res.unwrap();
                }
                buffer
            }
        })
    });

    group.finish();
}
