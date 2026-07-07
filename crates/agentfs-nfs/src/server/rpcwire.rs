use anyhow::anyhow;
use std::io::Cursor;
use std::io::{Read, Write};
use tracing::{debug, error, trace, warn};

use super::context::RPCContext;
use super::rpc::*;
use super::xdr::*;

use super::mount;
use super::mount_handlers;

use super::nfs;
use super::nfs_handlers;
use super::transaction_tracker::{TransactionKey, TransactionLookup};

use super::portmap;
use super::portmap_handlers;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::DuplexStream;
use tokio::sync::mpsc;

// Information from RFC 5531
// https://datatracker.ietf.org/doc/html/rfc5531

const NFS_ACL_PROGRAM: u32 = 100227;
const NFS_ID_MAP_PROGRAM: u32 = 100270;
const NFS_METADATA_PROGRAM: u32 = 200024;

async fn handle_rpc(
    input: &mut impl Read,
    output: &mut impl Write,
    mut context: RPCContext,
) -> Result<bool, anyhow::Error> {
    let mut recv = rpc_msg::default();
    recv.deserialize(input)?;
    let xid = recv.xid;
    if let rpc_body::CALL(call) = recv.body {
        if let auth_flavor::AUTH_UNIX = call.cred.flavor {
            let mut auth = auth_unix::default();
            auth.deserialize(&mut Cursor::new(&call.cred.body))?;
            context.auth = auth;
        }

        let mut client_verifier = Vec::new();
        call.verf.serialize(&mut client_verifier)?;
        let mut procedure_args = Vec::new();
        input.read_to_end(&mut procedure_args)?;
        let transaction_key = TransactionKey::new(
            xid,
            call.prog,
            call.vers,
            call.proc,
            client_verifier,
            &procedure_args,
        );
        let transaction = context.transaction_tracker.begin(transaction_key);
        let transaction_guard = match transaction {
            TransactionLookup::New(guard) => guard,
            TransactionLookup::InProgress => {
                debug!(
                    "In-progress retransmission detected, xid: {}, client_addr: {}, call: {:?}",
                    xid, context.client_addr, call
                );
                return Ok(false);
            }
            TransactionLookup::Replay(reply) => {
                debug!(
                    "Replaying cached RPC reply, xid: {}, client_addr: {}, call: {:?}",
                    xid, context.client_addr, call
                );
                output.write_all(reply.as_ref())?;
                return Ok(true);
            }
        };

        if call.rpcvers != 2 {
            warn!("Invalid RPC version {} != 2", call.rpcvers);
            let mut reply = Vec::new();
            rpc_vers_mismatch(xid).serialize(&mut reply)?;
            transaction_guard.complete(reply.clone());
            output.write_all(&reply)?;
            return Ok(true);
        }

        let mut handler_output = Vec::new();
        let mut args_cursor = Cursor::new(procedure_args.as_slice());
        let res = {
            if call.prog == nfs::PROGRAM {
                nfs_handlers::handle_nfs(xid, call, &mut args_cursor, &mut handler_output, &context)
                    .await
            } else if call.prog == portmap::PROGRAM {
                portmap_handlers::handle_portmap(
                    xid,
                    call,
                    &mut args_cursor,
                    &mut handler_output,
                    &context,
                )
            } else if call.prog == mount::PROGRAM {
                mount_handlers::handle_mount(
                    xid,
                    call,
                    &mut args_cursor,
                    &mut handler_output,
                    &context,
                )
                .await
            } else if call.prog == NFS_ACL_PROGRAM
                || call.prog == NFS_ID_MAP_PROGRAM
                || call.prog == NFS_METADATA_PROGRAM
            {
                trace!("ignoring NFS_ACL packet");
                prog_unavail_reply_message(xid).serialize(&mut handler_output)?;
                Ok(())
            } else {
                warn!(
                    "Unknown RPC Program number {} != {}",
                    call.prog,
                    nfs::PROGRAM
                );
                prog_unavail_reply_message(xid).serialize(&mut handler_output)?;
                Ok(())
            }
        }
        .map(|_| true);
        if res.is_ok() {
            transaction_guard.complete(handler_output.clone());
            output.write_all(&handler_output)?;
        }
        res
    } else {
        error!("Unexpectedly received a Reply instead of a Call");
        Err(anyhow!("Bad RPC Call format"))
    }
}

/// RFC 1057 Section 10
/// When RPC messages are passed on top of a byte stream transport
/// protocol (like TCP), it is necessary to delimit one message from
/// another in order to detect and possibly recover from protocol errors.
/// This is called record marking (RM).  Sun uses this RM/TCP/IP
/// transport for passing RPC messages on TCP streams.  One RPC message
/// fits into one RM record.
///
/// A record is composed of one or more record fragments.  A record
/// fragment is a four-byte header followed by 0 to (2**31) - 1 bytes of
/// fragment data.  The bytes encode an unsigned binary number; as with
/// XDR integers, the byte order is from highest to lowest.  The number
/// encodes two values -- a boolean which indicates whether the fragment
/// is the last fragment of the record (bit value 1 implies the fragment
/// is the last fragment) and a 31-bit unsigned binary value which is the
/// length in bytes of the fragment's data.  The boolean value is the
/// highest-order bit of the header; the length is the 31 low-order bits.
/// (Note that this record specification is NOT in XDR standard form!)
async fn read_fragment(
    socket: &mut DuplexStream,
    append_to: &mut Vec<u8>,
) -> Result<bool, anyhow::Error> {
    let mut header_buf = [0_u8; 4];
    socket.read_exact(&mut header_buf).await?;
    let fragment_header = u32::from_be_bytes(header_buf);
    let is_last = (fragment_header & (1 << 31)) > 0;
    let length = (fragment_header & ((1 << 31) - 1)) as usize;
    trace!("Reading fragment length:{}, last:{}", length, is_last);
    let start_offset = append_to.len();
    append_to.resize(append_to.len() + length, 0);
    socket.read_exact(&mut append_to[start_offset..]).await?;
    trace!(
        "Finishing Reading fragment length:{}, last:{}",
        length,
        is_last
    );
    Ok(is_last)
}

pub async fn write_fragment(
    socket: &mut tokio::net::TcpStream,
    buf: &[u8],
) -> Result<(), anyhow::Error> {
    // TODO: split into many fragments
    assert!(buf.len() < (1 << 31));
    // set the last flag
    let fragment_header = buf.len() as u32 + (1 << 31);
    let header_buf = u32::to_be_bytes(fragment_header);
    socket.write_all(&header_buf).await?;
    trace!("Writing fragment length:{}", buf.len());
    socket.write_all(buf).await?;
    Ok(())
}

pub type SocketMessageType = Result<Vec<u8>, anyhow::Error>;

/// The Socket Message Handler reads from a TcpStream and spawns off
/// subtasks to handle each message. replies are queued into the
/// reply_send_channel.
#[derive(Debug)]
pub struct SocketMessageHandler {
    cur_fragment: Vec<u8>,
    socket_receive_channel: DuplexStream,
    reply_send_channel: mpsc::UnboundedSender<SocketMessageType>,
    context: RPCContext,
}

impl SocketMessageHandler {
    /// Creates a new SocketMessageHandler with the receiver for queued message replies
    pub fn new(
        context: &RPCContext,
    ) -> (
        Self,
        DuplexStream,
        mpsc::UnboundedReceiver<SocketMessageType>,
    ) {
        let (socksend, sockrecv) = tokio::io::duplex(256000);
        let (msgsend, msgrecv) = mpsc::unbounded_channel();
        (
            Self {
                cur_fragment: Vec::new(),
                socket_receive_channel: sockrecv,
                reply_send_channel: msgsend,
                context: context.clone(),
            },
            socksend,
            msgrecv,
        )
    }

    /// Reads a fragment from the socket. This should be looped.
    pub async fn read(&mut self) -> Result<(), anyhow::Error> {
        let is_last =
            read_fragment(&mut self.socket_receive_channel, &mut self.cur_fragment).await?;
        if is_last {
            let fragment = std::mem::take(&mut self.cur_fragment);
            let context = self.context.clone();
            let send = self.reply_send_channel.clone();
            tokio::spawn(async move {
                let mut write_buf: Vec<u8> = Vec::new();
                let mut write_cursor = Cursor::new(&mut write_buf);
                let maybe_reply =
                    handle_rpc(&mut Cursor::new(fragment), &mut write_cursor, context).await;
                match maybe_reply {
                    Err(e) => {
                        error!("RPC Error: {:?}", e);
                        let _ = send.send(Err(e));
                    }
                    Ok(true) => {
                        let _ = std::io::Write::flush(&mut write_cursor);
                        let _ = send.send(Ok(write_buf));
                    }
                    Ok(false) => {
                        // do not reply
                    }
                }
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::AgentNFS;
    use crate::server::nfs;
    use crate::server::nfs_handlers::createmode3;
    use crate::server::rpc::{auth_flavor, call_body, opaque_auth, rpc_body, rpc_msg};
    use crate::server::transaction_tracker::{TransactionTracker, DEFAULT_REPLY_CACHE_CAPACITY};
    use crate::server::vfs::NFSFileSystem;
    use agentfs_core::{AgentFS as AgentSdk, AgentFSOptions, FileSystem};
    use std::sync::Arc;

    async fn test_context() -> (RPCContext, agentfs_core::fs::AgentFS) {
        let agent = AgentSdk::open(AgentFSOptions::ephemeral())
            .await
            .expect("open ephemeral AgentFS");
        agent
            .fs
            .chmod(1, 0o777)
            .await
            .expect("make root writable to unprivileged test user");
        let fs = agent.fs.clone();
        let nfs = AgentNFS::new(Arc::new(agent.fs));
        let vfs: Arc<dyn NFSFileSystem + Send + Sync> = Arc::new(nfs);
        let context = RPCContext {
            local_port: 11111,
            client_addr: "127.0.0.1:40001".to_string(),
            auth: crate::server::rpc::auth_unix {
                stamp: 0,
                machinename: b"test".to_vec(),
                uid: 1000,
                gid: 1000,
                gids: vec![1000],
            },
            vfs,
            export_name: Arc::new("/".to_string()),
            transaction_tracker: Arc::new(TransactionTracker::new(DEFAULT_REPLY_CACHE_CAPACITY)),
        };
        (context, fs)
    }

    fn verifier(body: &[u8]) -> opaque_auth {
        opaque_auth {
            flavor: auth_flavor::AUTH_NULL,
            body: body.to_vec(),
        }
    }

    fn serialize_guarded_create_call(
        xid: u32,
        root_fh: nfs::nfs_fh3,
        name: &[u8],
        verf: opaque_auth,
    ) -> Vec<u8> {
        let mut msg = Vec::new();
        rpc_msg {
            xid,
            body: rpc_body::CALL(call_body {
                rpcvers: 2,
                prog: nfs::PROGRAM,
                vers: nfs::VERSION,
                proc: 8,
                cred: opaque_auth::default(),
                verf,
            }),
        }
        .serialize(&mut msg)
        .expect("serialize RPC header");
        nfs::diropargs3 {
            dir: root_fh,
            name: name.into(),
        }
        .serialize(&mut msg)
        .expect("serialize CREATE dirops");
        createmode3::GUARDED
            .serialize(&mut msg)
            .expect("serialize CREATE mode");
        nfs::sattr3 {
            mode: nfs::set_mode3::mode(0o644),
            ..Default::default()
        }
        .serialize(&mut msg)
        .expect("serialize CREATE attrs");
        msg
    }

    fn serialize_getattr_call(xid: u32, fh: nfs::nfs_fh3, verf: opaque_auth) -> Vec<u8> {
        let mut msg = Vec::new();
        rpc_msg {
            xid,
            body: rpc_body::CALL(call_body {
                rpcvers: 2,
                prog: nfs::PROGRAM,
                vers: nfs::VERSION,
                proc: 1,
                cred: opaque_auth::default(),
                verf,
            }),
        }
        .serialize(&mut msg)
        .expect("serialize RPC header");
        fh.serialize(&mut msg).expect("serialize GETATTR fh");
        msg
    }

    async fn run_rpc(payload: &[u8], context: RPCContext) -> Vec<u8> {
        let mut input = Cursor::new(payload.to_vec());
        let mut output = Vec::new();
        let replied = handle_rpc(&mut input, &mut output, context)
            .await
            .expect("handle RPC");
        assert!(replied, "completed retransmissions must replay a reply");
        output
    }

    fn parse_nfs_status(reply: &[u8], xid: u32) -> nfs::nfsstat3 {
        let mut cursor = Cursor::new(reply.to_vec());
        let mut msg = rpc_msg::default();
        msg.deserialize(&mut cursor).expect("deserialize RPC reply");
        assert_eq!(msg.xid, xid);
        match msg.body {
            rpc_body::REPLY(crate::server::rpc::reply_body::MSG_ACCEPTED(
                crate::server::rpc::accepted_reply {
                    reply_data: crate::server::rpc::accept_body::SUCCESS,
                    ..
                },
            )) => {}
            other => panic!("unexpected RPC reply: {other:?}"),
        }
        let mut status = nfs::nfsstat3::NFS3_OK;
        status
            .deserialize(&mut cursor)
            .expect("deserialize NFS status");
        status
    }

    #[tokio::test]
    async fn completed_retransmission_replays_cached_reply_without_reexecuting_create() {
        let (context, fs) = test_context().await;
        let xid = 4100;
        let name = b"drc-replay-create";
        let call = serialize_guarded_create_call(
            xid,
            context.vfs.id_to_fh(1),
            name,
            verifier(b"client-verifier"),
        );

        let first_reply = run_rpc(&call, context.clone()).await;
        assert!(matches!(
            parse_nfs_status(&first_reply, xid),
            nfs::nfsstat3::NFS3_OK
        ));

        let retransmitted_reply = run_rpc(&call, context.clone()).await;
        assert_eq!(
            first_reply, retransmitted_reply,
            "completed retransmission should replay exact cached reply bytes"
        );
        assert!(matches!(
            parse_nfs_status(&retransmitted_reply, xid),
            nfs::nfsstat3::NFS3_OK
        ));
        assert!(
            FileSystem::lookup(&fs, 1, std::str::from_utf8(name).unwrap())
                .await
                .expect("lookup created file")
                .is_some()
        );
        println!(
            "replayed=true reconnect=false xid={xid} verifier=client-verifier side_effect_count=1"
        );
    }

    #[tokio::test]
    async fn drc_key_ignores_reconnect_source_port_churn() {
        let (context, _fs) = test_context().await;
        let xid = 4200;
        let name = b"drc-reconnect-create";
        let call = serialize_guarded_create_call(
            xid,
            context.vfs.id_to_fh(1),
            name,
            verifier(b"stable-client-verifier"),
        );
        let mut first_context = context.clone();
        first_context.client_addr = "127.0.0.1:40001".to_string();
        let mut reconnected_context = context.clone();
        reconnected_context.client_addr = "127.0.0.1:50002".to_string();

        let first_reply = run_rpc(&call, first_context).await;
        assert!(matches!(
            parse_nfs_status(&first_reply, xid),
            nfs::nfsstat3::NFS3_OK
        ));

        let retransmitted_reply = run_rpc(&call, reconnected_context).await;
        assert_eq!(
            first_reply, retransmitted_reply,
            "source-port churn must not change the DRC key"
        );
        assert!(matches!(
            parse_nfs_status(&retransmitted_reply, xid),
            nfs::nfsstat3::NFS3_OK
        ));
        println!(
            "replayed=true reconnect=true first_port=40001 second_port=50002 xid={xid} verifier=stable-client-verifier side_effect_count=1"
        );
    }

    #[tokio::test]
    async fn drc_key_separates_cross_procedure_xid_collisions() {
        let (context, _fs) = test_context().await;
        let xid = 4300;
        let verifier = verifier(b"shared-auth-none-verifier");
        let create = serialize_guarded_create_call(
            xid,
            context.vfs.id_to_fh(1),
            b"drc-cross-proc",
            verifier.clone(),
        );
        let getattr = serialize_getattr_call(xid, context.vfs.id_to_fh(1), verifier);

        let create_reply = run_rpc(&create, context.clone()).await;
        assert!(matches!(
            parse_nfs_status(&create_reply, xid),
            nfs::nfsstat3::NFS3_OK
        ));

        let getattr_reply = run_rpc(&getattr, context.clone()).await;
        assert_ne!(
            create_reply, getattr_reply,
            "same xid/verifier across different procedures must not replay cached bytes"
        );
        assert!(matches!(
            parse_nfs_status(&getattr_reply, xid),
            nfs::nfsstat3::NFS3_OK
        ));
        println!(
            "replayed=false collision=cross-procedure xid={xid} key_fields=prog,vers,proc,args_digest"
        );
    }

    #[tokio::test]
    async fn drc_key_separates_same_procedure_argument_collisions() {
        let (context, fs) = test_context().await;
        let xid = 4400;
        let verifier = verifier(b"shared-auth-none-verifier");
        let first_name = b"drc-create-a";
        let second_name = b"drc-create-b";
        let first = serialize_guarded_create_call(
            xid,
            context.vfs.id_to_fh(1),
            first_name,
            verifier.clone(),
        );
        let second =
            serialize_guarded_create_call(xid, context.vfs.id_to_fh(1), second_name, verifier);

        let first_reply = run_rpc(&first, context.clone()).await;
        assert!(matches!(
            parse_nfs_status(&first_reply, xid),
            nfs::nfsstat3::NFS3_OK
        ));

        let second_reply = run_rpc(&second, context.clone()).await;
        assert_ne!(
            first_reply, second_reply,
            "same xid/verifier/procedure with different args must not replay cached bytes"
        );
        assert!(matches!(
            parse_nfs_status(&second_reply, xid),
            nfs::nfsstat3::NFS3_OK
        ));
        assert!(
            FileSystem::lookup(&fs, 1, std::str::from_utf8(first_name).unwrap())
                .await
                .expect("lookup first file")
                .is_some()
        );
        assert!(
            FileSystem::lookup(&fs, 1, std::str::from_utf8(second_name).unwrap())
                .await
                .expect("lookup second file")
                .is_some()
        );
        println!(
            "replayed=false collision=same-procedure-different-args xid={xid} key_fields=args_digest"
        );
    }
}
