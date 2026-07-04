use super::context::RPCContext;
use super::rpcwire::*;
use super::transaction_tracker::TransactionTracker;
use super::vfs::NFSFileSystem;
use async_trait::async_trait;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use std::{io, net::IpAddr};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

/// A NFS Tcp Connection Handler
pub(crate) struct NFSTcpListener<T: NFSFileSystem + Send + Sync + 'static> {
    listener: TcpListener,
    port: u16,
    arcfs: Arc<T>,
    export_name: Arc<String>,
    transaction_tracker: Arc<TransactionTracker>,
}

pub fn generate_host_ip(hostnum: u16) -> String {
    format!(
        "127.88.{}.{}",
        ((hostnum >> 8) & 0xFF) as u8,
        (hostnum & 0xFF) as u8
    )
}

/// processes an established socket
async fn process_socket(
    mut socket: tokio::net::TcpStream,
    context: RPCContext,
    shutdown: CancellationToken,
) -> Result<(), anyhow::Error> {
    let (mut message_handler, mut socksend, mut msgrecvchan) = SocketMessageHandler::new(&context);
    let _ = socket.set_nodelay(true);

    let reader_shutdown = shutdown.clone();
    let reader_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = reader_shutdown.cancelled() => {
                    break;
                }
                result = message_handler.read() => {
                    if let Err(e) = result {
                        debug!("Message loop broken due to {:?}", e);
                        break;
                    }
                }
            }
        }
    });
    let result = loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                break Ok(());
            }
            _ = socket.readable() => {
                let mut buf = [0; 128000];

                match socket.try_read(&mut buf) {
                    Ok(0) => {
                        break Ok(());
                    }
                    Ok(n) => {
                        let _ = socksend.write_all(&buf[..n]).await;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        continue;
                    }
                    Err(e) => {
                        debug!("Message handling closed : {:?}", e);
                        break Err(e.into());
                    }
                }

            },
            reply = msgrecvchan.recv() => {
                match reply {
                    Some(Err(e)) => {
                        debug!("Message handling closed : {:?}", e);
                        break Err(e);
                    }
                    Some(Ok(msg)) => {
                        if let Err(e) = write_fragment(&mut socket, &msg).await {
                            error!("Write error {:?}", e);
                        }
                    }
                    None => {
                        break Err(anyhow::anyhow!("Unexpected socket context termination"));
                    }
                }
            }
        }
    };

    drop(socksend);
    match reader_task.await {
        Ok(()) => {}
        Err(error) => debug!("NFS socket reader task join error: {error:?}"),
    }

    result
}

#[async_trait]
pub(crate) trait NFSTcp: Send + Sync {
    /// Gets the true listening port. Useful if the bound port number is 0
    fn get_listen_port(&self) -> u16;

    /// Gets the true listening IP. Useful on windows when the IP may be random
    fn get_listen_ip(&self) -> IpAddr;

    /// Handles incoming connections until the cancellation token is triggered.
    async fn handle_until_cancelled(&self, shutdown: CancellationToken) -> io::Result<()>;
}

impl<T: NFSFileSystem + Send + Sync + 'static> NFSTcpListener<T> {
    /// Binds to a ipstr of the form [ip address]:port. For instance
    /// "127.0.0.1:12000". fs is an instance of an implementation
    /// of NFSFileSystem.
    pub async fn bind(ipstr: &str, fs: T) -> io::Result<NFSTcpListener<T>> {
        let (ip, port) = ipstr.split_once(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "IP Address must be of form ip:port",
            )
        })?;
        let port = port.parse::<u16>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "Port not in range 0..=65535",
            )
        })?;

        let arcfs: Arc<T> = Arc::new(fs);

        if ip == "auto" {
            let mut num_tries_left = 32;

            for try_ip in 1u16.. {
                let ip = generate_host_ip(try_ip);

                let result = NFSTcpListener::bind_internal(&ip, port, arcfs.clone()).await;

                match &result {
                    Err(_) => {
                        if num_tries_left == 0 {
                            return result;
                        } else {
                            num_tries_left -= 1;
                            continue;
                        }
                    }
                    Ok(_) => {
                        return result;
                    }
                }
            }
            unreachable!(); // Does not detect automatically that loop above never terminates.
        } else {
            // Otherwise, try this.
            NFSTcpListener::bind_internal(ip, port, arcfs).await
        }
    }

    async fn bind_internal(ip: &str, port: u16, arcfs: Arc<T>) -> io::Result<NFSTcpListener<T>> {
        let ipstr = format!("{ip}:{port}");
        let listener = TcpListener::bind(&ipstr).await?;
        debug!("Listening on {:?}", &ipstr);

        let port = match listener.local_addr().unwrap() {
            SocketAddr::V4(s) => s.port(),
            SocketAddr::V6(s) => s.port(),
        };
        Ok(NFSTcpListener {
            listener,
            port,
            arcfs,
            export_name: Arc::from("/".to_string()),
            transaction_tracker: Arc::new(TransactionTracker::new(Duration::from_secs(60))),
        })
    }

    /// Sets an optional NFS export name.
    ///
    /// - `export_name`: The desired export name without slashes.
    ///
    /// Example: Name `foo` results in the export path `/foo`.
    /// Default path is `/` if not set.
    pub fn with_export_name<S: AsRef<str>>(&mut self, export_name: S) {
        self.export_name = Arc::new(format!(
            "/{}",
            export_name
                .as_ref()
                .trim_end_matches('/')
                .trim_start_matches('/')
        ))
    }
}

#[async_trait]
impl<T: NFSFileSystem + Send + Sync + 'static> NFSTcp for NFSTcpListener<T> {
    /// Gets the true listening port. Useful if the bound port number is 0
    fn get_listen_port(&self) -> u16 {
        let addr = self.listener.local_addr().unwrap();
        addr.port()
    }

    fn get_listen_ip(&self) -> IpAddr {
        let addr = self.listener.local_addr().unwrap();
        addr.ip()
    }

    /// Handles incoming connections until the cancellation token is triggered.
    async fn handle_until_cancelled(&self, shutdown: CancellationToken) -> io::Result<()> {
        let mut connections = JoinSet::new();

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    debug!("NFS TCP listener received shutdown signal");
                    break;
                }
                accept_result = self.listener.accept() => {
                    let (socket, _) = accept_result?;
                    let context = RPCContext {
                        local_port: self.port,
                        client_addr: socket.peer_addr().unwrap().to_string(),
                        auth: super::rpc::auth_unix::default(),
                        vfs: self.arcfs.clone(),
                        export_name: self.export_name.clone(),
                        transaction_tracker: self.transaction_tracker.clone(),
                    };
                    debug!("Accepting connection from {}", context.client_addr);
                    debug!("Accepting socket {:?} {:?}", socket, context);
                    let connection_shutdown = shutdown.clone();
                    connections.spawn(async move {
                        process_socket(socket, context, connection_shutdown).await
                    });
                }
                joined = connections.join_next(), if !connections.is_empty() => {
                    log_connection_result(joined);
                }
            }
        }

        while let Some(joined) = connections.join_next().await {
            log_connection_result(Some(joined));
        }

        self.arcfs.finalize().await.map_err(io::Error::other)?;

        Ok(())
    }
}

fn log_connection_result(
    joined: Option<Result<Result<(), anyhow::Error>, tokio::task::JoinError>>,
) {
    match joined {
        Some(Ok(Ok(()))) => {}
        Some(Ok(Err(error))) => debug!("NFS connection task exited with error: {error:?}"),
        Some(Err(error)) => error!("NFS connection task join error: {error:?}"),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::AgentNFS;
    use crate::server::nfs;
    use crate::server::rpc::{self, accept_body, accepted_reply, reply_body, rpc_body, rpc_msg};
    use crate::server::vfs::NFSFileSystem;
    use crate::server::xdr::XDR;
    use agentfs_core::{AgentFS as AgentSdk, AgentFSOptions, FileSystem, DEFAULT_FILE_MODE};
    use std::io::Cursor;
    use tokio::io::AsyncReadExt;

    fn serialize_rpc_call(xid: u32, proc: u32) -> Vec<u8> {
        let mut msg = Vec::new();
        rpc_msg {
            xid,
            body: rpc_body::CALL(rpc::call_body {
                rpcvers: 2,
                prog: nfs::PROGRAM,
                vers: nfs::VERSION,
                proc,
                cred: rpc::opaque_auth::default(),
                verf: rpc::opaque_auth::default(),
            }),
        }
        .serialize(&mut msg)
        .expect("serialize RPC call");
        msg
    }

    fn parse_rpc_success(payload: Vec<u8>, xid: u32) -> Cursor<Vec<u8>> {
        let mut cursor = Cursor::new(payload);
        let mut reply = rpc_msg::default();
        reply
            .deserialize(&mut cursor)
            .expect("deserialize RPC reply");
        assert_eq!(reply.xid, xid);
        match reply.body {
            rpc_body::REPLY(reply_body::MSG_ACCEPTED(accepted_reply {
                reply_data: accept_body::SUCCESS,
                ..
            })) => {}
            other => panic!("unexpected RPC reply: {other:?}"),
        }
        cursor
    }

    async fn read_rpc_payload(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut header = [0_u8; 4];
        stream
            .read_exact(&mut header)
            .await
            .expect("read RPC record header");
        let fragment_header = u32::from_be_bytes(header);
        assert_ne!(
            fragment_header & (1 << 31),
            0,
            "reply should be final fragment"
        );
        let len = (fragment_header & ((1 << 31) - 1)) as usize;
        let mut payload = vec![0_u8; len];
        stream
            .read_exact(&mut payload)
            .await
            .expect("read RPC payload");
        payload
    }

    async fn send_null_and_getattr_probe(port: u16, root_fh: nfs::nfs_fh3) {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect to NFS listener");

        let null_call = serialize_rpc_call(100, 0);
        write_fragment(&mut stream, &null_call)
            .await
            .expect("write NULL call");
        let null_reply = read_rpc_payload(&mut stream).await;
        parse_rpc_success(null_reply, 100);

        let mut getattr_call = serialize_rpc_call(101, 1);
        root_fh
            .serialize(&mut getattr_call)
            .expect("serialize GETATTR root file handle");
        write_fragment(&mut stream, &getattr_call)
            .await
            .expect("write GETATTR call");
        let getattr_reply = read_rpc_payload(&mut stream).await;
        let mut cursor = parse_rpc_success(getattr_reply, 101);
        let mut status = nfs::nfsstat3::NFS3_OK;
        status
            .deserialize(&mut cursor)
            .expect("deserialize GETATTR status");
        assert!(matches!(status, nfs::nfsstat3::NFS3_OK));
    }

    async fn bind_available_local_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral probe port");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        port
    }

    #[tokio::test]
    async fn cancellation_drains_finalizes_and_releases_port_across_cycles() {
        std::env::set_var("AGENTFS_FUSE_WRITEBACK", "1");
        std::env::set_var("AGENTFS_OVERLAY_READS", "1");
        std::env::set_var("AGENTFS_BATCH_MS", "60000");
        std::env::set_var("AGENTFS_BATCH_BYTES", "1048576");
        std::env::set_var("AGENTFS_BATCH_GLOBAL_BYTES", "10485760");

        let port = bind_available_local_port().await;

        for cycle in 0..3 {
            let dir = tempfile::tempdir().expect("tempdir");
            let db_path = dir.path().join(format!("cycle-{cycle}.db"));
            let agent = AgentSdk::open(AgentFSOptions::with_path(
                db_path.to_str().expect("test DB path is UTF-8"),
            ))
            .await
            .expect("open file-backed AgentFS");
            let (_stats, file) = FileSystem::create_file(
                &agent.fs,
                1,
                "shutdown-drain.txt",
                DEFAULT_FILE_MODE,
                0,
                0,
            )
            .await
            .expect("create file");
            let payload = format!("cycle-{cycle}-pending-before-shutdown").into_bytes();
            file.pwrite(0, &payload)
                .await
                .expect("queue pending write before shutdown");
            drop(file);

            let nfs = AgentNFS::new(Arc::new(agent.fs));
            let root_fh = nfs.id_to_fh(1);
            let listener = NFSTcpListener::bind(&format!("127.0.0.1:{port}"), nfs)
                .await
                .expect("bind NFS listener on explicit port");
            let shutdown = CancellationToken::new();
            let server_shutdown = shutdown.clone();
            let server =
                tokio::spawn(async move { listener.handle_until_cancelled(server_shutdown).await });

            send_null_and_getattr_probe(port, root_fh).await;
            shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(3), server)
                .await
                .expect("server should stop within timeout")
                .expect("server task should not panic")
                .expect("server shutdown should succeed");

            let rebind = tokio::net::TcpListener::bind(("127.0.0.1", port)).await;
            assert!(
                rebind.is_ok(),
                "cycle {cycle}: cancelled NFS server should release port {port}"
            );
            drop(rebind);

            let reopened = AgentSdk::open(AgentFSOptions::with_path(
                db_path.to_str().expect("test DB path is UTF-8"),
            ))
            .await
            .expect("reopen DB after NFS shutdown");
            let stats = FileSystem::lookup(&reopened.fs, 1, "shutdown-drain.txt")
                .await
                .expect("lookup after reopen")
                .expect("file exists after reopen");
            let reopened_file = FileSystem::open(&reopened.fs, stats.ino, libc::O_RDONLY)
                .await
                .expect("open after reopen");
            let persisted = reopened_file
                .pread(0, payload.len() as u64)
                .await
                .expect("read after reopen");
            assert_eq!(
                persisted, payload,
                "cycle {cycle}: graceful NFS shutdown should drain pending writes"
            );
        }
    }
}
