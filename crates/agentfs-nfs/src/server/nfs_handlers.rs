#![allow(clippy::upper_case_acronyms)]
use super::context::RPCContext;
use super::nfs;
use super::permissions;
use super::rpc::*;
use super::xdr::*;
use agentfs_core::fs::MAX_NAME_LEN;
use agentfs_core::semantics::{access, AckDurability};
use byteorder::{ReadBytesExt, WriteBytesExt};
use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::cast::FromPrimitive;
use std::io::{Read, Write};
use tracing::{debug, error, trace, warn};
/*
program NFS_PROGRAM {
 version NFS_V3 {

    void
     NFSPROC3_NULL(void)                    = 0;

    GETATTR3res
     NFSPROC3_GETATTR(GETATTR3args)         = 1;

    SETATTR3res
     NFSPROC3_SETATTR(SETATTR3args)         = 2;

    LOOKUP3res
     NFSPROC3_LOOKUP(LOOKUP3args)           = 3;

    ACCESS3res
     NFSPROC3_ACCESS(ACCESS3args)           = 4;

    READLINK3res
     NFSPROC3_READLINK(READLINK3args)       = 5;

    READ3res
     NFSPROC3_READ(READ3args)               = 6;

    WRITE3res
     NFSPROC3_WRITE(WRITE3args)             = 7;

    CREATE3res
     NFSPROC3_CREATE(CREATE3args)           = 8;

    MKDIR3res
     NFSPROC3_MKDIR(MKDIR3args)             = 9;

    SYMLINK3res
     NFSPROC3_SYMLINK(SYMLINK3args)         = 10;

    MKNOD3res
     NFSPROC3_MKNOD(MKNOD3args)             = 11;

    REMOVE3res
     NFSPROC3_REMOVE(REMOVE3args)           = 12;

    RMDIR3res
     NFSPROC3_RMDIR(RMDIR3args)             = 13;

    RENAME3res
     NFSPROC3_RENAME(RENAME3args)           = 14;

    LINK3res
     NFSPROC3_LINK(LINK3args)               = 15;

    READDIR3res
     NFSPROC3_READDIR(READDIR3args)         = 16;

    READDIRPLUS3res
     NFSPROC3_READDIRPLUS(READDIRPLUS3args) = 17;

    FSSTAT3res
     NFSPROC3_FSSTAT(FSSTAT3args)           = 18;

    FSINFO3res
     NFSPROC3_FSINFO(FSINFO3args)           = 19;

    PATHCONF3res
     NFSPROC3_PATHCONF(PATHCONF3args)       = 20;

    COMMIT3res
     NFSPROC3_COMMIT(COMMIT3args)           = 21;

 } = 3;
} = 100003;
*/

#[allow(non_camel_case_types)]
#[allow(clippy::upper_case_acronyms)]
#[derive(Copy, Clone, Debug, FromPrimitive, ToPrimitive)]
enum NFSProgram {
    NFSPROC3_NULL = 0,
    NFSPROC3_GETATTR = 1,
    NFSPROC3_SETATTR = 2,
    NFSPROC3_LOOKUP = 3,
    NFSPROC3_ACCESS = 4,
    NFSPROC3_READLINK = 5,
    NFSPROC3_READ = 6,
    NFSPROC3_WRITE = 7,
    NFSPROC3_CREATE = 8,
    NFSPROC3_MKDIR = 9,
    NFSPROC3_SYMLINK = 10,
    NFSPROC3_MKNOD = 11,
    NFSPROC3_REMOVE = 12,
    NFSPROC3_RMDIR = 13,
    NFSPROC3_RENAME = 14,
    NFSPROC3_LINK = 15,
    NFSPROC3_READDIR = 16,
    NFSPROC3_READDIRPLUS = 17,
    NFSPROC3_FSSTAT = 18,
    NFSPROC3_FSINFO = 19,
    NFSPROC3_PATHCONF = 20,
    NFSPROC3_COMMIT = 21,
    INVALID = 22,
}

pub async fn handle_nfs(
    xid: u32,
    call: call_body,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    if call.vers != nfs::VERSION {
        warn!(
            "Invalid NFS Version number {} != {}",
            call.vers,
            nfs::VERSION
        );
        prog_mismatch_reply_message(xid, nfs::VERSION).serialize(output)?;
        return Ok(());
    }
    let prog = NFSProgram::from_u32(call.proc).unwrap_or(NFSProgram::INVALID);

    match prog {
        NFSProgram::NFSPROC3_NULL => nfsproc3_null(xid, input, output)?,
        NFSProgram::NFSPROC3_GETATTR => nfsproc3_getattr(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_LOOKUP => nfsproc3_lookup(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_READ => nfsproc3_read(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_FSINFO => nfsproc3_fsinfo(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_ACCESS => nfsproc3_access(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_PATHCONF => nfsproc3_pathconf(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_FSSTAT => nfsproc3_fsstat(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_READDIR => nfsproc3_readdir(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_READDIRPLUS => {
            nfsproc3_readdirplus(xid, input, output, context).await?
        }
        NFSProgram::NFSPROC3_WRITE => nfsproc3_write(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_CREATE => nfsproc3_create(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_SETATTR => nfsproc3_setattr(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_REMOVE => nfsproc3_remove(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_RMDIR => nfsproc3_remove(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_RENAME => nfsproc3_rename(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_MKDIR => nfsproc3_mkdir(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_SYMLINK => nfsproc3_symlink(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_READLINK => nfsproc3_readlink(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_MKNOD => nfsproc3_mknod(xid, input, output, context).await?,
        NFSProgram::NFSPROC3_LINK => nfsproc3_link(xid, input, output, context).await?,
        _ => {
            warn!("Unimplemented message {:?}", prog);
            proc_unavail_reply_message(xid).serialize(output)?;
        } /*
          NFSPROC3_COMMIT,
          INVALID*/
    }
    Ok(())
}

pub fn nfsproc3_null(
    xid: u32,
    _: &mut impl Read,
    output: &mut impl Write,
) -> Result<(), anyhow::Error> {
    debug!("nfsproc3_null({:?}) ", xid);
    let msg = make_success_reply(xid);
    debug!("\t{:?} --> {:?}", xid, msg);
    msg.serialize(output)?;
    Ok(())
}
/*
GETATTR3res NFSPROC3_GETATTR(GETATTR3args) = 1;
struct GETATTR3args {
  nfs_fh3  object;
};

struct GETATTR3resok {
  fattr3   obj_attributes;
};

union GETATTR3res switch (nfsstat3 status) {
 case NFS3_OK:
  GETATTR3resok  resok;
 default:
  void;
};
 */
pub async fn nfsproc3_getattr(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut handle = nfs::nfs_fh3::default();
    handle.deserialize(input)?;
    debug!("nfsproc3_getattr({:?},{:?}) ", xid, handle);

    let id = context.vfs.fh_to_id(&handle);
    // fail if unable to convert file handle
    if let Err(stat) = id {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        return Ok(());
    }
    let id = id.unwrap();
    match context.vfs.getattr(id).await {
        Ok(fh) => {
            debug!(" {:?} --> {:?}", xid, fh);
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            fh.serialize(output)?;
        }
        Err(stat) => {
            error!("getattr error {:?} --> {:?}", xid, stat);
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
        }
    }
    Ok(())
}

/*
 LOOKUP3res NFSPROC3_LOOKUP(LOOKUP3args) = 3;

 struct LOOKUP3args {
      diropargs3  what;
 };

 struct LOOKUP3resok {
      nfs_fh3      object;
      post_op_attr obj_attributes;
      post_op_attr dir_attributes;
 };

 struct LOOKUP3resfail {
      post_op_attr dir_attributes;
 };

 union LOOKUP3res switch (nfsstat3 status) {
 case NFS3_OK:
      LOOKUP3resok    resok;
 default:
      LOOKUP3resfail  resfail;
 };
*
*/
pub async fn nfsproc3_lookup(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut dirops = nfs::diropargs3::default();
    dirops.deserialize(input)?;
    debug!("nfsproc3_lookup({:?},{:?}) ", xid, dirops);

    let dirid = context.vfs.fh_to_id(&dirops.dir);
    // fail if unable to convert file handle
    if let Err(stat) = dirid {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::post_op_attr::Void.serialize(output)?;
        return Ok(());
    }
    let dirid = dirid.unwrap();

    let dir_attr_full = match context.vfs.getattr(dirid).await {
        Ok(v) => v,
        Err(stat) => {
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::post_op_attr::Void.serialize(output)?;
            return Ok(());
        }
    };

    let creds = permissions::credentials(&context.auth);
    let dir_stats = permissions::stats(&dir_attr_full);
    // Check execute (search) permission on directory
    if !access::may_search(&dir_stats, &creds) {
        debug!(
            "lookup permission denied for uid={} on directory",
            context.auth.uid
        );
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ACCES.serialize(output)?;
        nfs::post_op_attr::attributes(dir_attr_full).serialize(output)?;
        return Ok(());
    }

    let dir_attr = nfs::post_op_attr::attributes(dir_attr_full);
    match context.vfs.lookup(dirid, &dirops.name).await {
        Ok(fid) => {
            let obj_attr = match context.vfs.getattr(fid).await {
                Ok(v) => nfs::post_op_attr::attributes(v),
                Err(_) => nfs::post_op_attr::Void,
            };

            debug!("lookup success {:?} --> {:?}", xid, obj_attr);
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            context.vfs.id_to_fh(fid).serialize(output)?;
            obj_attr.serialize(output)?;
            dir_attr.serialize(output)?;
        }
        Err(stat) => {
            debug!("lookup error {:?}({:?}) --> {:?}", xid, dirops.name, stat);
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            dir_attr.serialize(output)?;
        }
    }
    Ok(())
}

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
struct READ3args {
    file: nfs::nfs_fh3,
    offset: nfs::offset3,
    count: nfs::count3,
}
XDRStruct!(READ3args, file, offset, count);

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
struct READ3resok {
    file_attributes: nfs::post_op_attr,
    count: nfs::count3,
    eof: bool,
    data: Vec<u8>,
}
XDRStruct!(READ3resok, file_attributes, count, eof, data);
/*
READ3res NFSPROC3_READ(READ3args) = 6;

struct READ3args {
   nfs_fh3  file;
   offset3  offset;
   count3   count;
};

struct READ3resok {
   post_op_attr   file_attributes;
   count3         count;
   bool           eof;
   opaque         data<>;
};

struct READ3resfail {
   post_op_attr   file_attributes;
};

union READ3res switch (nfsstat3 status) {
case NFS3_OK:
   READ3resok   resok;
default:
   READ3resfail resfail;
};
 */
pub async fn nfsproc3_read(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut args = READ3args::default();
    args.deserialize(input)?;
    debug!("nfsproc3_read({:?},{:?}) ", xid, args);

    let id = context.vfs.fh_to_id(&args.file);
    if let Err(stat) = id {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::post_op_attr::Void.serialize(output)?;
        return Ok(());
    }
    let id = id.unwrap();

    let attr = match context.vfs.getattr(id).await {
        Ok(v) => v,
        Err(stat) => {
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::post_op_attr::Void.serialize(output)?;
            return Ok(());
        }
    };

    let creds = permissions::credentials(&context.auth);
    let stats = permissions::stats(&attr);
    // Check read permission
    if !access::may_read(&stats, &creds) {
        debug!("read permission denied for uid={}", context.auth.uid);
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ACCES.serialize(output)?;
        nfs::post_op_attr::attributes(attr).serialize(output)?;
        return Ok(());
    }

    let obj_attr = nfs::post_op_attr::attributes(attr);
    match context.vfs.read(id, args.offset, args.count).await {
        Ok((bytes, eof)) => {
            let res = READ3resok {
                file_attributes: obj_attr,
                count: bytes.len() as u32,
                eof,
                data: bytes,
            };
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            res.serialize(output)?;
        }
        Err(stat) => {
            error!("read error {:?} --> {:?}", xid, stat);
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            obj_attr.serialize(output)?;
        }
    }
    Ok(())
}

/*

  FSINFO3res NFSPROC3_FSINFO(FSINFO3args) = 19;

  const FSF3_LINK        = 0x0001;
  const FSF3_SYMLINK     = 0x0002;
  const FSF3_HOMOGENEOUS = 0x0008;
  const FSF3_CANSETTIME  = 0x0010;

  struct FSINFOargs {
       nfs_fh3   fsroot;
  };

  struct FSINFO3resok {
       post_op_attr obj_attributes;
       uint32       rtmax;
       uint32       rtpref;
       uint32       rtmult;
       uint32       wtmax;
       uint32       wtpref;
       uint32       wtmult;
       uint32       dtpref;
       size3        maxfilesize;
       nfstime3     time_delta;
       uint32       properties;
  };

  struct FSINFO3resfail {
       post_op_attr obj_attributes;
  };

  union FSINFO3res switch (nfsstat3 status) {
  case NFS3_OK:
       FSINFO3resok   resok;
  default:
       FSINFO3resfail resfail;
  };
*/

pub async fn nfsproc3_fsinfo(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut handle = nfs::nfs_fh3::default();
    handle.deserialize(input)?;
    debug!("nfsproc3_fsinfo({:?},{:?}) ", xid, handle);

    let id = context.vfs.fh_to_id(&handle);
    // fail if unable to convert file handle
    if let Err(stat) = id {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::post_op_attr::Void.serialize(output)?;
        return Ok(());
    }
    let id = id.unwrap();

    match context.vfs.fsinfo(id).await {
        Ok(fsinfo) => {
            debug!(" {:?} --> {:?}", xid, fsinfo);
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            fsinfo.serialize(output)?;
        }
        Err(stat) => {
            error!("fsinfo error {:?} --> {:?}", xid, stat);
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
        }
    }
    Ok(())
}

/*

 ACCESS3res NFSPROC3_ACCESS(ACCESS3args) = 4;


 struct ACCESS3args {
      nfs_fh3  object;
      uint32   access;
 };

 struct ACCESS3resok {
      post_op_attr   obj_attributes;
      uint32         access;
 };

 struct ACCESS3resfail {
      post_op_attr   obj_attributes;
 };

 union ACCESS3res switch (nfsstat3 status) {
 case NFS3_OK:
      ACCESS3resok   resok;
 default:
      ACCESS3resfail resfail;
 };
*/

pub async fn nfsproc3_access(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut handle = nfs::nfs_fh3::default();
    handle.deserialize(input)?;
    let mut requested_access: u32 = 0;
    requested_access.deserialize(input)?;
    debug!(
        "nfsproc3_access({:?},{:?},{:?})",
        xid, handle, requested_access
    );

    let id = context.vfs.fh_to_id(&handle);
    // fail if unable to convert file handle
    if let Err(stat) = id {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::post_op_attr::Void.serialize(output)?;
        return Ok(());
    }
    let id = id.unwrap();

    let attr = match context.vfs.getattr(id).await {
        Ok(v) => v,
        Err(stat) => {
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::post_op_attr::Void.serialize(output)?;
            return Ok(());
        }
    };

    // Compute access based on auth credentials and file attributes
    let granted_access = permissions::compute_access(&context.auth, &attr, requested_access);

    debug!(
        " {:?} ---> requested={:?}, granted={:?}",
        xid, requested_access, granted_access
    );
    make_success_reply(xid).serialize(output)?;
    nfs::nfsstat3::NFS3_OK.serialize(output)?;
    nfs::post_op_attr::attributes(attr).serialize(output)?;
    granted_access.serialize(output)?;
    Ok(())
}

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
struct PATHCONF3resok {
    obj_attributes: nfs::post_op_attr,
    linkmax: u32,
    name_max: u32,
    no_trunc: bool,
    chown_restricted: bool,
    case_insensitive: bool,
    case_preserving: bool,
}
XDRStruct!(
    PATHCONF3resok,
    obj_attributes,
    linkmax,
    name_max,
    no_trunc,
    chown_restricted,
    case_insensitive,
    case_preserving
);
/*

     PATHCONF3res NFSPROC3_PATHCONF(PATHCONF3args) = 20;

     struct PATHCONF3args {
          nfs_fh3   object;
     };

     struct PATHCONF3resok {
          post_op_attr obj_attributes;
          uint32       linkmax;
          uint32       name_max;
          bool         no_trunc;
          bool         chown_restricted;
          bool         case_insensitive;
          bool         case_preserving;
     };

     struct PATHCONF3resfail {
          post_op_attr obj_attributes;
     };

     union PATHCONF3res switch (nfsstat3 status) {
     case NFS3_OK:
          PATHCONF3resok   resok;
     default:
          PATHCONF3resfail resfail;
     };
*/
pub async fn nfsproc3_pathconf(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut handle = nfs::nfs_fh3::default();
    handle.deserialize(input)?;
    debug!("nfsproc3_pathconf({:?},{:?})", xid, handle);

    let id = context.vfs.fh_to_id(&handle);
    // fail if unable to convert file handle
    if let Err(stat) = id {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::post_op_attr::Void.serialize(output)?;
        return Ok(());
    }
    let id = id.unwrap();

    let obj_attr = match context.vfs.getattr(id).await {
        Ok(v) => nfs::post_op_attr::attributes(v),
        Err(_) => nfs::post_op_attr::Void,
    };
    let res = PATHCONF3resok {
        obj_attributes: obj_attr,
        linkmax: 0,
        name_max: MAX_NAME_LEN as u32,
        no_trunc: true,
        chown_restricted: true,
        case_insensitive: false,
        case_preserving: true,
    };
    debug!(" {:?} ---> {:?}", xid, res);
    make_success_reply(xid).serialize(output)?;
    nfs::nfsstat3::NFS3_OK.serialize(output)?;
    res.serialize(output)?;
    Ok(())
}

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
struct FSSTAT3resok {
    obj_attributes: nfs::post_op_attr,
    tbytes: nfs::size3,
    fbytes: nfs::size3,
    abytes: nfs::size3,
    tfiles: nfs::size3,
    ffiles: nfs::size3,
    afiles: nfs::size3,
    invarsec: u32,
}
XDRStruct!(
    FSSTAT3resok,
    obj_attributes,
    tbytes,
    fbytes,
    abytes,
    tfiles,
    ffiles,
    afiles,
    invarsec
);

/*
 FSSTAT3res NFSPROC3_FSSTAT(FSSTAT3args) = 18;

     struct FSSTAT3args {
          nfs_fh3   fsroot;
     };

     struct FSSTAT3resok {
          post_op_attr obj_attributes;
          size3        tbytes;
          size3        fbytes;
          size3        abytes;
          size3        tfiles;
          size3        ffiles;
          size3        afiles;
          uint32       invarsec;
     };

     struct FSSTAT3resfail {
          post_op_attr obj_attributes;
     };

     union FSSTAT3res switch (nfsstat3 status) {
     case NFS3_OK:
          FSSTAT3resok   resok;
     default:
          FSSTAT3resfail resfail;
     };

*/

pub async fn nfsproc3_fsstat(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut handle = nfs::nfs_fh3::default();
    handle.deserialize(input)?;
    debug!("nfsproc3_fsstat({:?},{:?}) ", xid, handle);
    let id = context.vfs.fh_to_id(&handle);
    // fail if unable to convert file handle
    if let Err(stat) = id {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::post_op_attr::Void.serialize(output)?;
        return Ok(());
    }
    let id = id.unwrap();

    let obj_attr = match context.vfs.getattr(id).await {
        Ok(v) => nfs::post_op_attr::attributes(v),
        Err(_) => nfs::post_op_attr::Void,
    };
    let res = FSSTAT3resok {
        obj_attributes: obj_attr,
        tbytes: 1024 * 1024 * 1024 * 1024,
        fbytes: 1024 * 1024 * 1024 * 1024,
        abytes: 1024 * 1024 * 1024 * 1024,
        tfiles: 1024 * 1024 * 1024,
        ffiles: 1024 * 1024 * 1024,
        afiles: 1024 * 1024 * 1024,
        invarsec: u32::MAX,
    };
    make_success_reply(xid).serialize(output)?;
    nfs::nfsstat3::NFS3_OK.serialize(output)?;
    debug!(" {:?} ---> {:?}", xid, res);
    res.serialize(output)?;
    Ok(())
}

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
struct READDIRPLUS3args {
    dir: nfs::nfs_fh3,
    cookie: nfs::cookie3,
    cookieverf: nfs::cookieverf3,
    dircount: nfs::count3,
    maxcount: nfs::count3,
}
XDRStruct!(
    READDIRPLUS3args,
    dir,
    cookie,
    cookieverf,
    dircount,
    maxcount
);

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
struct entry3 {
    fileid: nfs::fileid3,
    name: nfs::filename3,
    cookie: nfs::cookie3,
}
XDRStruct!(entry3, fileid, name, cookie);

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
struct READDIR3args {
    dir: nfs::nfs_fh3,
    cookie: nfs::cookie3,
    cookieverf: nfs::cookieverf3,
    dircount: nfs::count3,
}
XDRStruct!(READDIR3args, dir, cookie, cookieverf, dircount);

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
struct entryplus3 {
    fileid: nfs::fileid3,
    name: nfs::filename3,
    cookie: nfs::cookie3,
    name_attributes: nfs::post_op_attr,
    name_handle: nfs::post_op_fh3,
}
XDRStruct!(
    entryplus3,
    fileid,
    name,
    cookie,
    name_attributes,
    name_handle
);
/*

      READDIRPLUS3res NFSPROC3_READDIRPLUS(READDIRPLUS3args) = 17;

      struct READDIRPLUS3args {
           nfs_fh3      dir;
           cookie3      cookie;
           cookieverf3  cookieverf;
           count3       dircount;
           count3       maxcount;
      };


      struct dirlistplus3 {
           entryplus3   *entries;
           bool         eof;
      };

      struct READDIRPLUS3resok {
           post_op_attr dir_attributes;
           cookieverf3  cookieverf;
           dirlistplus3 reply;
      };
   struct READDIRPLUS3resfail {
           post_op_attr dir_attributes;
      };
*/
pub async fn nfsproc3_readdirplus(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut args = READDIRPLUS3args::default();
    args.deserialize(input)?;
    debug!("nfsproc3_readdirplus({:?},{:?}) ", xid, args);

    let dirid = context.vfs.fh_to_id(&args.dir);
    // fail if unable to convert file handle
    if let Err(stat) = dirid {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::post_op_attr::Void.serialize(output)?;
        return Ok(());
    }
    let dirid = dirid.unwrap();
    let dir_attr_maybe = context.vfs.getattr(dirid).await;

    let dir_attr_full = match dir_attr_maybe {
        Ok(v) => v,
        Err(stat) => {
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::post_op_attr::Void.serialize(output)?;
            return Ok(());
        }
    };

    let creds = permissions::credentials(&context.auth);
    let dir_stats = permissions::stats(&dir_attr_full);
    // Check read permission on directory
    if !access::may_read(&dir_stats, &creds) {
        debug!(
            "readdirplus permission denied for uid={} on directory",
            context.auth.uid
        );
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ACCES.serialize(output)?;
        nfs::post_op_attr::attributes(dir_attr_full).serialize(output)?;
        return Ok(());
    }

    let dir_attr = nfs::post_op_attr::attributes(dir_attr_full);
    let dir_attr_maybe: Result<nfs::fattr3, nfs::nfsstat3> = Ok(dir_attr_full);

    let dirversion = if let Ok(ref dir_attr) = dir_attr_maybe {
        let cvf_version = (dir_attr.mtime.seconds as u64) << 32 | (dir_attr.mtime.nseconds as u64);
        cvf_version.to_be_bytes()
    } else {
        nfs::cookieverf3::default()
    };
    debug!(" -- Dir attr {:?}", dir_attr);
    debug!(" -- Dir version {:?}", dirversion);
    let has_version = args.cookieverf != nfs::cookieverf3::default();
    // initial call should hve empty cookie verf
    // subsequent calls should have cvf_version as defined above
    // which is based off the mtime.
    //
    // TODO: This is *far* too aggressive. and unnecessary.
    // The client should maintain this correctly typically.
    //
    // The way cookieverf is handled is quite interesting...
    //
    // There are 2 notes in the RFC of interest:
    // 1. If the
    // server detects that the cookie is no longer valid, the
    // server will reject the READDIR request with the status,
    // NFS3ERR_BAD_COOKIE. The client should be careful to
    // avoid holding directory entry cookies across operations
    // that modify the directory contents, such as REMOVE and
    // CREATE.
    //
    // 2. One implementation of the cookie-verifier mechanism might
    //  be for the server to use the modification time of the
    //  directory. This might be overly restrictive, however. A
    //  better approach would be to record the time of the last
    //  directory modification that changed the directory
    //  organization in a way that would make it impossible to
    //  reliably interpret a cookie. Servers in which directory
    //  cookies are always valid are free to use zero as the
    //  verifier always.
    //
    //  Basically, as long as the cookie is "kinda" intepretable,
    //  we should keep accepting it.
    //  On testing, the Mac NFS client pretty much expects that
    //  especially on highly concurrent modifications to the directory.
    //
    //  1. If part way through a directory enumeration we fail with BAD_COOKIE
    //  if the directory contents change, the client listing may fail resulting
    //  in a "no such file or directory" error.
    //  2. if we cache readdir results. i.e. we think of a readdir as two parts
    //     a. enumerating everything first
    //     b. the cookie is then used to paginate the enumeration
    //     we can run into file time synchronization issues. i.e. while one
    //     listing occurs and another file is touched, the listing may report
    //     an outdated file status.
    //
    //     This cache also appears to have to be *quite* long lasting
    //     as the client may hold on to a directory enumerator
    //     with unbounded time.
    //
    //  Basically, if we think about how linux directory listing works
    //  is that you just get an enumerator. There is no mechanic available for
    //  "restarting" a pagination and this enumerator is assumed to be valid
    //  even across directory modifications and should reflect changes
    //  immediately.
    //
    //  The best solution is simply to really completely avoid sending
    //  BAD_COOKIE all together and to ignore the cookie mechanism.
    //
    /*if args.cookieverf != nfs::cookieverf3::default() && args.cookieverf != dirversion {
        info!(" -- Dir version mismatch. Received {:?}", args.cookieverf);
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_BAD_COOKIE.serialize(output)?;
        dir_attr.serialize(output)?;
        return Ok(());
    }*/
    // subtract off the final entryplus* field (which must be false) and the eof
    let max_bytes_allowed = (args.maxcount as usize).saturating_sub(128);
    // args.dircount is bytes of just fileid, name, cookie.
    // This is hard to ballpark, so we just divide it by 16
    let estimated_max_results = args.dircount / 16;
    let max_dircount_bytes = args.dircount as usize;
    let mut ctr = 0;
    match context
        .vfs
        .readdir(dirid, args.cookie, (estimated_max_results as usize).max(1))
        .await
    {
        Ok(result) => {
            // we count dir_count seperately as it is just a subset of fields
            let mut accumulated_dircount: usize = 0;
            let mut all_entries_written = true;

            // this is a wrapper around a writer that also just counts the number of bytes
            // written
            let mut counting_output = super::write_counter::WriteCounter::new(output);

            make_success_reply(xid).serialize(&mut counting_output)?;
            nfs::nfsstat3::NFS3_OK.serialize(&mut counting_output)?;
            dir_attr.serialize(&mut counting_output)?;
            dirversion.serialize(&mut counting_output)?;
            for entry in result.entries {
                let obj_attr = entry.attr;
                let handle =
                    nfs::post_op_fh3::handle(context.vfs.id_to_readdirplus_fh(entry.fileid));

                let entry = entryplus3 {
                    fileid: entry.fileid,
                    name: entry.name,
                    cookie: entry.fileid,
                    name_attributes: nfs::post_op_attr::attributes(obj_attr),
                    name_handle: handle,
                };
                // write the entry into a buffer first
                let mut write_buf: Vec<u8> = Vec::new();
                let mut write_cursor = std::io::Cursor::new(&mut write_buf);
                // true flag for the entryplus3* to mark that this contains an entry
                true.serialize(&mut write_cursor)?;
                entry.serialize(&mut write_cursor)?;
                write_cursor.flush()?;
                let added_dircount = std::mem::size_of::<nfs::fileid3>()                   // fileid
                                    + std::mem::size_of::<u32>() + entry.name.len()  // name
                                    + std::mem::size_of::<nfs::cookie3>(); // cookie
                let added_output_bytes = write_buf.len();
                // check if we can write without hitting the limits
                if added_output_bytes + counting_output.bytes_written() < max_bytes_allowed
                    && added_dircount + accumulated_dircount < max_dircount_bytes
                {
                    trace!("  -- dirent {:?}", entry);
                    // commit the entry
                    ctr += 1;
                    counting_output.write_all(&write_buf)?;
                    accumulated_dircount += added_dircount;
                    trace!(
                        "  -- lengths: {:?} / {:?} {:?} / {:?}",
                        accumulated_dircount,
                        max_dircount_bytes,
                        counting_output.bytes_written(),
                        max_bytes_allowed
                    );
                } else {
                    trace!(" -- insufficient space. truncating");
                    all_entries_written = false;
                    break;
                }
            }
            // false flag for the final entryplus* linked list
            false.serialize(&mut counting_output)?;
            // eof flag is only valid here if we wrote everything
            if all_entries_written {
                debug!("  -- readdir eof {:?}", result.end);
                result.end.serialize(&mut counting_output)?;
            } else {
                debug!("  -- readdir eof {:?}", false);
                false.serialize(&mut counting_output)?;
            }
            debug!(
                "readir {}, has_version {},  start at {}, flushing {} entries, complete {}",
                dirid, has_version, args.cookie, ctr, all_entries_written
            );
        }
        Err(stat) => {
            error!("readdir error {:?} --> {:?} ", xid, stat);
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            dir_attr.serialize(output)?;
        }
    };
    Ok(())
}

pub async fn nfsproc3_readdir(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut args = READDIR3args::default();
    args.deserialize(input)?;
    debug!("nfsproc3_readdir({:?},{:?}) ", xid, args);

    let dirid = context.vfs.fh_to_id(&args.dir);
    // fail if unable to convert file handle
    if let Err(stat) = dirid {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::post_op_attr::Void.serialize(output)?;
        return Ok(());
    }
    let dirid = dirid.unwrap();
    let dir_attr_maybe = context.vfs.getattr(dirid).await;

    let dir_attr_full = match dir_attr_maybe {
        Ok(v) => v,
        Err(stat) => {
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::post_op_attr::Void.serialize(output)?;
            return Ok(());
        }
    };

    let creds = permissions::credentials(&context.auth);
    let dir_stats = permissions::stats(&dir_attr_full);
    // Check read permission on directory
    if !access::may_read(&dir_stats, &creds) {
        debug!(
            "readdir permission denied for uid={} on directory",
            context.auth.uid
        );
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ACCES.serialize(output)?;
        nfs::post_op_attr::attributes(dir_attr_full).serialize(output)?;
        return Ok(());
    }

    let dir_attr = nfs::post_op_attr::attributes(dir_attr_full);
    let dir_attr_maybe: Result<nfs::fattr3, nfs::nfsstat3> = Ok(dir_attr_full);

    let dirversion = if let Ok(ref dir_attr) = dir_attr_maybe {
        let cvf_version = (dir_attr.mtime.seconds as u64) << 32 | (dir_attr.mtime.nseconds as u64);
        cvf_version.to_be_bytes()
    } else {
        nfs::cookieverf3::default()
    };
    debug!(" -- Dir attr {:?}", dir_attr);
    debug!(" -- Dir version {:?}", dirversion);
    let has_version = args.cookieverf != nfs::cookieverf3::default();
    // subtract off the final entryplus* field (which must be false) and the eof
    let max_bytes_allowed = (args.dircount as usize).saturating_sub(128);
    // args.dircount is bytes of just fileid, name, cookie.
    // This is hard to ballpark, so we just divide it by 16
    let estimated_max_results = args.dircount / 16;
    let mut ctr = 0;
    match context
        .vfs
        .readdir_simple(dirid, args.cookie, (estimated_max_results as usize).max(1))
        .await
    {
        Ok(result) => {
            // we count dir_count seperately as it is just a subset of fields
            let mut accumulated_dircount: usize = 0;
            let mut all_entries_written = true;

            // this is a wrapper around a writer that also just counts the number of bytes
            // written
            let mut counting_output = super::write_counter::WriteCounter::new(output);

            make_success_reply(xid).serialize(&mut counting_output)?;
            nfs::nfsstat3::NFS3_OK.serialize(&mut counting_output)?;
            dir_attr.serialize(&mut counting_output)?;
            dirversion.serialize(&mut counting_output)?;
            for entry in result.entries {
                let entry = entry3 {
                    fileid: entry.fileid,
                    name: entry.name,
                    cookie: entry.fileid,
                };
                // write the entry into a buffer first
                let mut write_buf: Vec<u8> = Vec::new();
                let mut write_cursor = std::io::Cursor::new(&mut write_buf);
                // true flag for the entryplus3* to mark that this contains an entry
                true.serialize(&mut write_cursor)?;
                entry.serialize(&mut write_cursor)?;
                write_cursor.flush()?;
                let added_dircount = std::mem::size_of::<nfs::fileid3>()                   // fileid
                                    + std::mem::size_of::<u32>() + entry.name.len()  // name
                                    + std::mem::size_of::<nfs::cookie3>(); // cookie
                let added_output_bytes = write_buf.len();
                // check if we can write without hitting the limits
                if added_output_bytes + counting_output.bytes_written() < max_bytes_allowed {
                    trace!("  -- dirent {:?}", entry);
                    // commit the entry
                    ctr += 1;
                    counting_output.write_all(&write_buf)?;
                    accumulated_dircount += added_dircount;
                    trace!(
                        "  -- lengths: {:?} / {:?} / {:?}",
                        accumulated_dircount,
                        counting_output.bytes_written(),
                        max_bytes_allowed
                    );
                } else {
                    trace!(" -- insufficient space. truncating");
                    all_entries_written = false;
                    break;
                }
            }
            // false flag for the final entryplus* linked list
            false.serialize(&mut counting_output)?;
            // eof flag is only valid here if we wrote everything
            if all_entries_written {
                debug!("  -- readdir eof {:?}", result.end);
                result.end.serialize(&mut counting_output)?;
            } else {
                debug!("  -- readdir eof {:?}", false);
                false.serialize(&mut counting_output)?;
            }
            debug!(
                "readir {}, has_version {},  start at {}, flushing {} entries, complete {}",
                dirid, has_version, args.cookie, ctr, all_entries_written
            );
        }
        Err(stat) => {
            error!("readdir error {:?} --> {:?} ", xid, stat);
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            dir_attr.serialize(output)?;
        }
    };
    Ok(())
}

#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, FromPrimitive, ToPrimitive)]
#[repr(u32)]
pub enum stable_how {
    #[default]
    UNSTABLE = 0,
    DATA_SYNC = 1,
    FILE_SYNC = 2,
}
XDREnumSerde!(stable_how);

fn ack_durability_for_write_request(_stable: u32) -> AckDurability {
    // D3's current NFS policy is FILE_SYNC-honest for every WRITE: even if a
    // client asks for UNSTABLE, this server commits before acknowledging and
    // reports FILE_SYNC. UNSTABLE+COMMIT can later opt in by changing this one
    // mapping.
    AckDurability::Committed
}

fn stable_how_for_ack_durability(durability: AckDurability) -> stable_how {
    match durability {
        AckDurability::Volatile => stable_how::UNSTABLE,
        AckDurability::Committed => stable_how::FILE_SYNC,
    }
}

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
struct WRITE3args {
    file: nfs::nfs_fh3,
    offset: nfs::offset3,
    count: nfs::count3,
    stable: u32,
    data: Vec<u8>,
}
XDRStruct!(WRITE3args, file, offset, count, stable, data);

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
struct WRITE3resok {
    file_wcc: nfs::wcc_data,
    count: nfs::count3,
    committed: stable_how,
    verf: nfs::writeverf3,
}
XDRStruct!(WRITE3resok, file_wcc, count, committed, verf);
/*
enum stable_how {
    UNSTABLE = 0,
    DATA_SYNC = 1,
    FILE_SYNC = 2
};


struct WRITE3args {
    nfs_fh3 file;
    offset3 offset;
    count3 count;
    stable_how stable;
    opaque data<>;
};

struct WRITE3resok {
    wcc_data file_wcc;
    count3 count;
    stable_how committed;
    writeverf3 verf;
};


struct WRITE3resfail {
    wcc_data file_wcc;
};


union WRITE3res switch (nfsstat3 status) {
    case NFS3_OK:
        WRITE3resok resok;
    default:
        WRITE3resfail resfail;
};

 */
pub async fn nfsproc3_write(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut args = WRITE3args::default();
    args.deserialize(input)?;
    debug!("nfsproc3_write({:?},...) ", xid);
    // sanity check the length
    if args.data.len() != args.count as usize {
        garbage_args_reply_message(xid).serialize(output)?;
        return Ok(());
    }

    let id = context.vfs.fh_to_id(&args.file);
    if let Err(stat) = id {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::wcc_data::default().serialize(output)?;
        return Ok(());
    }
    let id = id.unwrap();

    // get the object attributes before the write
    let attr = match context.vfs.getattr(id).await {
        Ok(v) => v,
        Err(stat) => {
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
            return Ok(());
        }
    };

    let creds = permissions::credentials(&context.auth);
    let stats = permissions::stats(&attr);

    // Check write permission. NFSv3 is stateless and has no OPEN RPC, but the
    // file handle returned by CREATE represents the client's open write path.
    // Honor write authority captured in that handle so git loose objects can
    // be created with a read-only final mode and still receive writes through
    // the same handle; fresh LOOKUP handles still fall back to mode checks.
    if !context.vfs.fh_has_write_authority(&args.file, id) && !access::may_write(&stats, &creds) {
        debug!("write permission denied for uid={}", context.auth.uid);
        let pre_obj_attr = nfs::pre_op_attr::attributes(nfs::wcc_attr {
            size: attr.size,
            mtime: attr.mtime,
            ctime: attr.ctime,
        });
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ACCES.serialize(output)?;
        nfs::wcc_data {
            before: pre_obj_attr,
            after: nfs::post_op_attr::attributes(attr),
        }
        .serialize(output)?;
        return Ok(());
    }

    let pre_obj_attr = nfs::pre_op_attr::attributes(nfs::wcc_attr {
        size: attr.size,
        mtime: attr.mtime,
        ctime: attr.ctime,
    });

    let requested_durability = ack_durability_for_write_request(args.stable);
    match context
        .vfs
        .write(id, args.offset, &args.data, requested_durability)
        .await
    {
        Ok((mut fattr, ack_durability)) => {
            // POSIX: Clear SUID/SGID bits when a non-root user writes to a file.
            let written_stats = permissions::stats(&fattr);
            let cleared_mode = permissions::without_killpriv(&written_stats, &creds, fattr.mode);
            if cleared_mode != fattr.mode {
                let clear_sattr = nfs::sattr3 {
                    mode: nfs::set_mode3::mode(cleared_mode),
                    ..Default::default()
                };
                if let Ok(updated) = context.vfs.setattr(id, clear_sattr).await {
                    fattr = updated;
                }
            }
            debug!("write success {:?} --> {:?}", xid, fattr);
            let res = WRITE3resok {
                file_wcc: nfs::wcc_data {
                    before: pre_obj_attr,
                    after: nfs::post_op_attr::attributes(fattr),
                },
                count: args.count,
                committed: stable_how_for_ack_durability(ack_durability),
                verf: context.vfs.serverid(),
            };
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            res.serialize(output)?;
        }
        Err(stat) => {
            error!("write error {:?} --> {:?}", xid, stat);
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
        }
    }
    Ok(())
}

#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Debug, Default, FromPrimitive, ToPrimitive)]
#[repr(u32)]
pub enum createmode3 {
    #[default]
    UNCHECKED = 0,
    GUARDED = 1,
    EXCLUSIVE = 2,
}
XDREnumSerde!(createmode3);
/*
CREATE3res NFSPROC3_CREATE(CREATE3args) = 8;

      enum createmode3 {
           UNCHECKED = 0,
           GUARDED   = 1,
           EXCLUSIVE = 2
      };

      union createhow3 switch (createmode3 mode) {
      case UNCHECKED:
      case GUARDED:
           sattr3       obj_attributes;
      case EXCLUSIVE:
           createverf3  verf;
      };

      struct CREATE3args {
           diropargs3   where;
           createhow3   how;
      };

      struct CREATE3resok {
           post_op_fh3   obj;
           post_op_attr  obj_attributes;
           wcc_data      dir_wcc;
      };

      struct CREATE3resfail {
           wcc_data      dir_wcc;
      };

      union CREATE3res switch (nfsstat3 status) {
      case NFS3_OK:
           CREATE3resok    resok;
      default:
           CREATE3resfail  resfail;
      };
*/

pub async fn nfsproc3_create(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut dirops = nfs::diropargs3::default();
    dirops.deserialize(input)?;
    let mut createhow = createmode3::default();
    createhow.deserialize(input)?;

    debug!("nfsproc3_create({:?}, {:?}, {:?}) ", xid, dirops, createhow);

    // find the directory we are supposed to create the
    // new file in
    let dirid = context.vfs.fh_to_id(&dirops.dir);
    if let Err(stat) = dirid {
        // directory does not exist
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::wcc_data::default().serialize(output)?;
        error!("Directory does not exist");
        return Ok(());
    }
    // found the directory, get the attributes
    let dirid = dirid.unwrap();

    // get the object attributes before the write
    let dir_attr = match context.vfs.getattr(dirid).await {
        Ok(v) => v,
        Err(stat) => {
            error!("Cannot stat directory");
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
            return Ok(());
        }
    };

    let pre_dir_attr = nfs::pre_op_attr::attributes(nfs::wcc_attr {
        size: dir_attr.size,
        mtime: dir_attr.mtime,
        ctime: dir_attr.ctime,
    });

    let creds = permissions::credentials(&context.auth);
    let dir_stats = permissions::stats(&dir_attr);
    // Check write and execute permission on parent directory
    if !access::may_modify_directory(&dir_stats, &creds) {
        debug!(
            "create permission denied for uid={} on directory",
            context.auth.uid
        );
        let post_dir_attr = match context.vfs.getattr(dirid).await {
            Ok(v) => nfs::post_op_attr::attributes(v),
            Err(_) => nfs::post_op_attr::Void,
        };
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ACCES.serialize(output)?;
        nfs::wcc_data {
            before: pre_dir_attr,
            after: post_dir_attr,
        }
        .serialize(output)?;
        return Ok(());
    }

    let mut target_attributes = nfs::sattr3::default();

    match createhow {
        createmode3::UNCHECKED => {
            target_attributes.deserialize(input)?;
            debug!("create unchecked {:?}", target_attributes);
        }
        createmode3::GUARDED => {
            target_attributes.deserialize(input)?;
            debug!("create guarded {:?}", target_attributes);
            if context.vfs.lookup(dirid, &dirops.name).await.is_ok() {
                // file exists. Fail with NFS3ERR_EXIST.
                // Re-read dir attributes
                // for post op attr
                let post_dir_attr = match context.vfs.getattr(dirid).await {
                    Ok(v) => nfs::post_op_attr::attributes(v),
                    Err(_) => nfs::post_op_attr::Void,
                };

                make_success_reply(xid).serialize(output)?;
                nfs::nfsstat3::NFS3ERR_EXIST.serialize(output)?;
                nfs::wcc_data {
                    before: pre_dir_attr,
                    after: post_dir_attr,
                }
                .serialize(output)?;
                return Ok(());
            }
        }
        createmode3::EXCLUSIVE => {
            debug!("create exclusive");
        }
    }

    let fid: Result<nfs::fileid3, nfs::nfsstat3>;
    let postopattr: nfs::post_op_attr;
    // fill in the fid and post op attr here
    if matches!(createhow, createmode3::EXCLUSIVE) {
        // the API for exclusive is very slightly different
        // We are not returning a post op attribute
        fid = context
            .vfs
            .create_exclusive(dirid, &dirops.name, &context.auth)
            .await;
        postopattr = nfs::post_op_attr::Void;
    } else {
        // create!
        let res = context
            .vfs
            .create(dirid, &dirops.name, target_attributes, &context.auth)
            .await;
        fid = res.map(|x| x.0);
        postopattr = if let Ok((_, fattr)) = res {
            nfs::post_op_attr::attributes(fattr)
        } else {
            nfs::post_op_attr::Void
        };
    }

    // Re-read dir attributes for post op attr
    let post_dir_attr = match context.vfs.getattr(dirid).await {
        Ok(v) => nfs::post_op_attr::attributes(v),
        Err(_) => nfs::post_op_attr::Void,
    };
    let wcc_res = nfs::wcc_data {
        before: pre_dir_attr,
        after: post_dir_attr,
    };

    match fid {
        Ok(fid) => {
            debug!("create success --> {:?}, {:?}", fid, postopattr);
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            // serialize CREATE3resok
            let fh = context.vfs.id_to_write_fh(fid);
            nfs::post_op_fh3::handle(fh).serialize(output)?;
            postopattr.serialize(output)?;
            wcc_res.serialize(output)?;
        }
        Err(e) => {
            error!("create error --> {:?}", e);
            // serialize CREATE3resfail
            make_success_reply(xid).serialize(output)?;
            e.serialize(output)?;
            wcc_res.serialize(output)?;
        }
    }

    Ok(())
}

#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Debug, Default)]
#[repr(u32)]
pub enum sattrguard3 {
    #[default]
    Void,
    obj_ctime(nfs::nfstime3),
}
XDRBoolUnion!(sattrguard3, obj_ctime, nfs::nfstime3);

#[allow(non_camel_case_types)]
#[derive(Clone, Debug, Default)]
struct SETATTR3args {
    object: nfs::nfs_fh3,
    new_attribute: nfs::sattr3,
    guard: sattrguard3,
}
XDRStruct!(SETATTR3args, object, new_attribute, guard);

/*
    SETATTR3res NFSPROC3_SETATTR(SETATTR3args) = 2;

      union sattrguard3 switch (bool check) {
      case TRUE:
         nfstime3  obj_ctime;
      case FALSE:
         void;
      };

      struct SETATTR3args {
         nfs_fh3      object;
         sattr3       new_attributes;
         sattrguard3  guard;
      };

      struct SETATTR3resok {
         wcc_data  obj_wcc;
      };

      struct SETATTR3resfail {
         wcc_data  obj_wcc;
      };
      union SETATTR3res switch (nfsstat3 status) {
      case NFS3_OK:
         SETATTR3resok   resok;
      default:
         SETATTR3resfail resfail;
      };
*/

pub async fn nfsproc3_setattr(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut args = SETATTR3args::default();
    args.deserialize(input)?;
    debug!("nfsproc3_setattr({:?},{:?}) ", xid, args);

    let id = context.vfs.fh_to_id(&args.object);
    // fail if unable to convert file handle
    if let Err(stat) = id {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        return Ok(());
    }
    let id = id.unwrap();

    let attr = match context.vfs.getattr(id).await {
        Ok(v) => v,
        Err(stat) => {
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
            return Ok(());
        }
    };

    let ctime = attr.ctime;
    let pre_op_attr = nfs::pre_op_attr::attributes(nfs::wcc_attr {
        size: attr.size,
        mtime: attr.mtime,
        ctime: attr.ctime,
    });

    let creds = permissions::credentials(&context.auth);
    let stats = permissions::stats(&attr);
    let original_change = permissions::attr_change(&args.new_attribute);
    let mut authorized_change = original_change;
    if original_change.size && context.vfs.fh_has_write_authority(&args.object, id) {
        authorized_change.size = false;
    }

    if let Err(error) = access::setattr_allowed(&stats, &creds, &authorized_change) {
        debug!(
            "setattr permission denied for uid={} on ino={}: {}",
            context.auth.uid, id, error
        );
        make_success_reply(xid).serialize(output)?;
        permissions::denial_status(&error).serialize(output)?;
        nfs::wcc_data {
            before: pre_op_attr,
            after: nfs::post_op_attr::attributes(attr),
        }
        .serialize(output)?;
        return Ok(());
    }

    if let Some(mode) = permissions::normalize_setattr_mode(&stats, &creds, &original_change) {
        args.new_attribute.mode = nfs::set_mode3::mode(mode);
    }

    // POSIX: writes/truncates and non-root owner/group changes clear SUID/SGID
    // unless this SETATTR also supplies an explicit mode.
    if (original_change.size || original_change.uid.is_some() || original_change.gid.is_some())
        && original_change.mode.is_none()
    {
        let cleared_mode = permissions::without_killpriv(&stats, &creds, attr.mode);
        if cleared_mode != attr.mode {
            args.new_attribute.mode = nfs::set_mode3::mode(cleared_mode);
        }
    }

    // handle the guard
    match args.guard {
        sattrguard3::Void => {}
        sattrguard3::obj_ctime(c) => {
            if c.seconds != ctime.seconds || c.nseconds != ctime.nseconds {
                make_success_reply(xid).serialize(output)?;
                nfs::nfsstat3::NFS3ERR_NOT_SYNC.serialize(output)?;
                nfs::wcc_data::default().serialize(output)?;
                return Ok(());
            }
        }
    }

    match context.vfs.setattr(id, args.new_attribute).await {
        Ok(post_op_attr) => {
            debug!(" setattr success {:?} --> {:?}", xid, post_op_attr);
            let wcc_res = nfs::wcc_data {
                before: pre_op_attr,
                after: nfs::post_op_attr::attributes(post_op_attr),
            };
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            wcc_res.serialize(output)?;
        }
        Err(stat) => {
            error!("setattr error {:?} --> {:?}", xid, stat);
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
        }
    }
    Ok(())
}

/*
      REMOVE3res NFSPROC3_REMOVE(REMOVE3args) = 12;

      struct REMOVE3args {
           diropargs3  object;
      };

      struct REMOVE3resok {
           wcc_data    dir_wcc;
      };

      struct REMOVE3resfail {
           wcc_data    dir_wcc;
      };

      union REMOVE3res switch (nfsstat3 status) {
      case NFS3_OK:
           REMOVE3resok   resok;
      default:
           REMOVE3resfail resfail;
      };

      RMDIR is basically identically structured
*/

pub async fn nfsproc3_remove(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut dirops = nfs::diropargs3::default();
    dirops.deserialize(input)?;

    debug!("nfsproc3_remove({:?}, {:?}) ", xid, dirops);

    // find the directory with the file
    let dirid = context.vfs.fh_to_id(&dirops.dir);
    if let Err(stat) = dirid {
        // directory does not exist
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::wcc_data::default().serialize(output)?;
        error!("Directory does not exist");
        return Ok(());
    }
    let dirid = dirid.unwrap();

    // get the object attributes before the write
    let dir_attr = match context.vfs.getattr(dirid).await {
        Ok(v) => v,
        Err(stat) => {
            error!("Cannot stat directory");
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
            return Ok(());
        }
    };

    let pre_dir_attr = nfs::pre_op_attr::attributes(nfs::wcc_attr {
        size: dir_attr.size,
        mtime: dir_attr.mtime,
        ctime: dir_attr.ctime,
    });

    let creds = permissions::credentials(&context.auth);
    let dir_stats = permissions::stats(&dir_attr);
    // Check permission to delete entry (includes sticky bit check)
    let remove_allowed = match context.vfs.lookup(dirid, &dirops.name).await {
        Ok(entry_id) => match context.vfs.getattr(entry_id).await {
            Ok(entry_attr) => {
                let entry_stats = permissions::stats(&entry_attr);
                access::sticky_delete_ok(&dir_stats, &entry_stats, &creds)
            }
            Err(_) => access::may_modify_directory(&dir_stats, &creds),
        },
        Err(_) => access::may_modify_directory(&dir_stats, &creds),
    };

    if !remove_allowed {
        debug!(
            "remove permission denied for uid={} on directory",
            context.auth.uid
        );
        let post_dir_attr = match context.vfs.getattr(dirid).await {
            Ok(v) => nfs::post_op_attr::attributes(v),
            Err(_) => nfs::post_op_attr::Void,
        };
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ACCES.serialize(output)?;
        nfs::wcc_data {
            before: pre_dir_attr,
            after: post_dir_attr,
        }
        .serialize(output)?;
        return Ok(());
    }

    // delete!
    let res = context.vfs.remove(dirid, &dirops.name).await;

    // Re-read dir attributes for post op attr
    let post_dir_attr = match context.vfs.getattr(dirid).await {
        Ok(v) => nfs::post_op_attr::attributes(v),
        Err(_) => nfs::post_op_attr::Void,
    };
    let wcc_res = nfs::wcc_data {
        before: pre_dir_attr,
        after: post_dir_attr,
    };

    match res {
        Ok(()) => {
            debug!("remove success");
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            wcc_res.serialize(output)?;
        }
        Err(e) => {
            error!("remove error {:?} --> {:?}", xid, e);
            // serialize CREATE3resfail
            make_success_reply(xid).serialize(output)?;
            e.serialize(output)?;
            wcc_res.serialize(output)?;
        }
    }

    Ok(())
}

/*
 RENAME3res NFSPROC3_RENAME(RENAME3args) = 14;

      struct RENAME3args {
           diropargs3   from;
           diropargs3   to;
      };

      struct RENAME3resok {
           wcc_data     fromdir_wcc;
           wcc_data     todir_wcc;
      };

      struct RENAME3resfail {
           wcc_data     fromdir_wcc;
           wcc_data     todir_wcc;
      };

      union RENAME3res switch (nfsstat3 status) {
      case NFS3_OK:
           RENAME3resok   resok;
      default:
           RENAME3resfail resfail;
      };
*/

pub async fn nfsproc3_rename(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut fromdirops = nfs::diropargs3::default();
    let mut todirops = nfs::diropargs3::default();
    fromdirops.deserialize(input)?;
    todirops.deserialize(input)?;

    debug!(
        "nfsproc3_rename({:?}, {:?}, {:?}) ",
        xid, fromdirops, todirops
    );

    // find the from directory
    let from_dirid = context.vfs.fh_to_id(&fromdirops.dir);
    if let Err(stat) = from_dirid {
        // directory does not exist
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::wcc_data::default().serialize(output)?;
        error!("Directory does not exist");
        return Ok(());
    }

    // find the to directory
    let to_dirid = context.vfs.fh_to_id(&todirops.dir);
    if let Err(stat) = to_dirid {
        // directory does not exist
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::wcc_data::default().serialize(output)?;
        error!("Directory does not exist");
        return Ok(());
    }

    // found the directory, get the attributes
    let from_dirid = from_dirid.unwrap();
    let to_dirid = to_dirid.unwrap();

    // get the object attributes before the write
    let from_dir_attr = match context.vfs.getattr(from_dirid).await {
        Ok(v) => v,
        Err(stat) => {
            error!("Cannot stat directory");
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
            return Ok(());
        }
    };

    let pre_from_dir_attr = nfs::pre_op_attr::attributes(nfs::wcc_attr {
        size: from_dir_attr.size,
        mtime: from_dir_attr.mtime,
        ctime: from_dir_attr.ctime,
    });

    // get the object attributes before the write
    let to_dir_attr = match context.vfs.getattr(to_dirid).await {
        Ok(v) => v,
        Err(stat) => {
            error!("Cannot stat directory");
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
            return Ok(());
        }
    };

    let pre_to_dir_attr = nfs::pre_op_attr::attributes(nfs::wcc_attr {
        size: to_dir_attr.size,
        mtime: to_dir_attr.mtime,
        ctime: to_dir_attr.ctime,
    });

    let creds = permissions::credentials(&context.auth);
    let from_dir_stats = permissions::stats(&from_dir_attr);
    let to_dir_stats = permissions::stats(&to_dir_attr);
    // Check permission on source directory (includes sticky bit check)
    let from_allowed = match context.vfs.lookup(from_dirid, &fromdirops.name).await {
        Ok(entry_id) => match context.vfs.getattr(entry_id).await {
            Ok(entry_attr) => {
                let entry_stats = permissions::stats(&entry_attr);
                access::sticky_rename_from_ok(
                    &from_dir_stats,
                    &entry_stats,
                    &creds,
                    from_dirid != to_dirid,
                )
            }
            Err(_) => access::may_modify_directory(&from_dir_stats, &creds),
        },
        Err(_) => access::may_modify_directory(&from_dir_stats, &creds),
    };

    if !from_allowed {
        debug!(
            "rename permission denied for uid={} on source directory",
            context.auth.uid
        );
        let post_from_dir_attr = match context.vfs.getattr(from_dirid).await {
            Ok(v) => nfs::post_op_attr::attributes(v),
            Err(_) => nfs::post_op_attr::Void,
        };
        let post_to_dir_attr = match context.vfs.getattr(to_dirid).await {
            Ok(v) => nfs::post_op_attr::attributes(v),
            Err(_) => nfs::post_op_attr::Void,
        };
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ACCES.serialize(output)?;
        nfs::wcc_data {
            before: pre_from_dir_attr,
            after: post_from_dir_attr,
        }
        .serialize(output)?;
        nfs::wcc_data {
            before: pre_to_dir_attr,
            after: post_to_dir_attr,
        }
        .serialize(output)?;
        return Ok(());
    }

    // Check permission on target directory (includes sticky bit check if dest exists)
    let to_allowed = match context.vfs.lookup(to_dirid, &todirops.name).await {
        Ok(entry_id) => match context.vfs.getattr(entry_id).await {
            Ok(entry_attr) => {
                let entry_stats = permissions::stats(&entry_attr);
                access::sticky_delete_ok(&to_dir_stats, &entry_stats, &creds)
            }
            Err(_) => access::may_modify_directory(&to_dir_stats, &creds),
        },
        // Dest doesn't exist, just need write+execute on target dir
        Err(_) => access::may_modify_directory(&to_dir_stats, &creds),
    };

    if !to_allowed {
        debug!(
            "rename permission denied for uid={} on target directory",
            context.auth.uid
        );
        let post_from_dir_attr = match context.vfs.getattr(from_dirid).await {
            Ok(v) => nfs::post_op_attr::attributes(v),
            Err(_) => nfs::post_op_attr::Void,
        };
        let post_to_dir_attr = match context.vfs.getattr(to_dirid).await {
            Ok(v) => nfs::post_op_attr::attributes(v),
            Err(_) => nfs::post_op_attr::Void,
        };
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ACCES.serialize(output)?;
        nfs::wcc_data {
            before: pre_from_dir_attr,
            after: post_from_dir_attr,
        }
        .serialize(output)?;
        nfs::wcc_data {
            before: pre_to_dir_attr,
            after: post_to_dir_attr,
        }
        .serialize(output)?;
        return Ok(());
    }

    // rename!
    let res = context
        .vfs
        .rename(from_dirid, &fromdirops.name, to_dirid, &todirops.name)
        .await;

    // Re-read dir attributes for post op attr
    let post_from_dir_attr = match context.vfs.getattr(from_dirid).await {
        Ok(v) => nfs::post_op_attr::attributes(v),
        Err(_) => nfs::post_op_attr::Void,
    };
    let post_to_dir_attr = match context.vfs.getattr(to_dirid).await {
        Ok(v) => nfs::post_op_attr::attributes(v),
        Err(_) => nfs::post_op_attr::Void,
    };
    let from_wcc_res = nfs::wcc_data {
        before: pre_from_dir_attr,
        after: post_from_dir_attr,
    };

    let to_wcc_res = nfs::wcc_data {
        before: pre_to_dir_attr,
        after: post_to_dir_attr,
    };

    match res {
        Ok(()) => {
            debug!("rename success");
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            from_wcc_res.serialize(output)?;
            to_wcc_res.serialize(output)?;
        }
        Err(e) => {
            error!("rename error {:?} --> {:?}", xid, e);
            // serialize CREATE3resfail
            make_success_reply(xid).serialize(output)?;
            e.serialize(output)?;
            from_wcc_res.serialize(output)?;
            to_wcc_res.serialize(output)?;
        }
    }

    Ok(())
}

/*
     MKDIR3res NFSPROC3_MKDIR(MKDIR3args) = 9;

     struct MKDIR3args {
          diropargs3   where;
          sattr3       attributes;
     };

     struct MKDIR3resok {
          post_op_fh3   obj;
          post_op_attr  obj_attributes;
          wcc_data      dir_wcc;
     };

     struct MKDIR3resfail {
          wcc_data      dir_wcc;
     };

     union MKDIR3res switch (nfsstat3 status) {
     case NFS3_OK:
          MKDIR3resok   resok;
     default:
          MKDIR3resfail resfail;
     };

*/

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
struct MKDIR3args {
    dirops: nfs::diropargs3,
    attributes: nfs::sattr3,
}
XDRStruct!(MKDIR3args, dirops, attributes);

pub async fn nfsproc3_mkdir(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut args = MKDIR3args::default();
    args.deserialize(input)?;

    debug!("nfsproc3_mkdir({:?}, {:?}) ", xid, args);

    // find the directory we are supposed to create the
    // new file in
    let dirid = context.vfs.fh_to_id(&args.dirops.dir);
    if let Err(stat) = dirid {
        // directory does not exist
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::wcc_data::default().serialize(output)?;
        error!("Directory does not exist");
        return Ok(());
    }
    // found the directory, get the attributes
    let dirid = dirid.unwrap();

    // get the object attributes before the write
    let dir_attr = match context.vfs.getattr(dirid).await {
        Ok(v) => v,
        Err(stat) => {
            error!("Cannot stat directory");
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
            return Ok(());
        }
    };

    let pre_dir_attr = nfs::pre_op_attr::attributes(nfs::wcc_attr {
        size: dir_attr.size,
        mtime: dir_attr.mtime,
        ctime: dir_attr.ctime,
    });

    let creds = permissions::credentials(&context.auth);
    let dir_stats = permissions::stats(&dir_attr);
    // Check write and execute permission on parent directory
    if !access::may_modify_directory(&dir_stats, &creds) {
        debug!(
            "mkdir permission denied for uid={} on directory",
            context.auth.uid
        );
        let post_dir_attr = match context.vfs.getattr(dirid).await {
            Ok(v) => nfs::post_op_attr::attributes(v),
            Err(_) => nfs::post_op_attr::Void,
        };
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ACCES.serialize(output)?;
        nfs::wcc_data {
            before: pre_dir_attr,
            after: post_dir_attr,
        }
        .serialize(output)?;
        return Ok(());
    }

    let res = context
        .vfs
        .mkdir(dirid, &args.dirops.name, args.attributes, &context.auth)
        .await;

    // Re-read dir attributes for post op attr
    let post_dir_attr = match context.vfs.getattr(dirid).await {
        Ok(v) => nfs::post_op_attr::attributes(v),
        Err(_) => nfs::post_op_attr::Void,
    };
    let wcc_res = nfs::wcc_data {
        before: pre_dir_attr,
        after: post_dir_attr,
    };

    match res {
        Ok((fid, fattr)) => {
            debug!("mkdir success --> {:?}, {:?}", fid, fattr);
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            // serialize CREATE3resok
            let fh = context.vfs.id_to_fh(fid);
            nfs::post_op_fh3::handle(fh).serialize(output)?;
            nfs::post_op_attr::attributes(fattr).serialize(output)?;
            wcc_res.serialize(output)?;
        }
        Err(e) => {
            debug!("mkdir error {:?} --> {:?}", xid, e);
            // serialize CREATE3resfail
            make_success_reply(xid).serialize(output)?;
            e.serialize(output)?;
            wcc_res.serialize(output)?;
        }
    }

    Ok(())
}

/*
      SYMLINK3res NFSPROC3_SYMLINK(SYMLINK3args) = 10;

      struct symlinkdata3 {
           sattr3    symlink_attributes;
           nfspath3  symlink_data;
      };

      struct SYMLINK3args {
           diropargs3    where;
           symlinkdata3  symlink;
      };

      struct SYMLINK3resok {
           post_op_fh3   obj;
           post_op_attr  obj_attributes;
           wcc_data      dir_wcc;
      };

      struct SYMLINK3resfail {
           wcc_data      dir_wcc;
      };

      union SYMLINK3res switch (nfsstat3 status) {
      case NFS3_OK:
           SYMLINK3resok   resok;
      default:
           SYMLINK3resfail resfail;
      };
*/

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
struct SYMLINK3args {
    dirops: nfs::diropargs3,
    symlink: nfs::symlinkdata3,
}
XDRStruct!(SYMLINK3args, dirops, symlink);

pub async fn nfsproc3_symlink(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut args = SYMLINK3args::default();
    args.deserialize(input)?;

    debug!("nfsproc3_symlink({:?}, {:?}) ", xid, args);

    // find the directory we are supposed to create the
    // new file in
    let dirid = context.vfs.fh_to_id(&args.dirops.dir);
    if let Err(stat) = dirid {
        // directory does not exist
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::wcc_data::default().serialize(output)?;
        error!("Directory does not exist");
        return Ok(());
    }
    // found the directory, get the attributes
    let dirid = dirid.unwrap();

    // get the object attributes before the write
    let dir_attr = match context.vfs.getattr(dirid).await {
        Ok(v) => v,
        Err(stat) => {
            error!("Cannot stat directory");
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
            return Ok(());
        }
    };

    let pre_dir_attr = nfs::pre_op_attr::attributes(nfs::wcc_attr {
        size: dir_attr.size,
        mtime: dir_attr.mtime,
        ctime: dir_attr.ctime,
    });

    let creds = permissions::credentials(&context.auth);
    let dir_stats = permissions::stats(&dir_attr);
    // Check write and execute permission on parent directory
    if !access::may_modify_directory(&dir_stats, &creds) {
        debug!(
            "symlink permission denied for uid={} on directory",
            context.auth.uid
        );
        let post_dir_attr = match context.vfs.getattr(dirid).await {
            Ok(v) => nfs::post_op_attr::attributes(v),
            Err(_) => nfs::post_op_attr::Void,
        };
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ACCES.serialize(output)?;
        nfs::wcc_data {
            before: pre_dir_attr,
            after: post_dir_attr,
        }
        .serialize(output)?;
        return Ok(());
    }

    let res = context
        .vfs
        .symlink(
            dirid,
            &args.dirops.name,
            &args.symlink.symlink_data,
            &args.symlink.symlink_attributes,
            &context.auth,
        )
        .await;

    // Re-read dir attributes for post op attr
    let post_dir_attr = match context.vfs.getattr(dirid).await {
        Ok(v) => nfs::post_op_attr::attributes(v),
        Err(_) => nfs::post_op_attr::Void,
    };
    let wcc_res = nfs::wcc_data {
        before: pre_dir_attr,
        after: post_dir_attr,
    };

    match res {
        Ok((fid, fattr)) => {
            debug!("symlink success --> {:?}, {:?}", fid, fattr);
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            // serialize CREATE3resok
            let fh = context.vfs.id_to_fh(fid);
            nfs::post_op_fh3::handle(fh).serialize(output)?;
            nfs::post_op_attr::attributes(fattr).serialize(output)?;
            wcc_res.serialize(output)?;
        }
        Err(e) => {
            debug!("symlink error --> {:?}", e);
            // serialize CREATE3resfail
            make_success_reply(xid).serialize(output)?;
            e.serialize(output)?;
            wcc_res.serialize(output)?;
        }
    }

    Ok(())
}

/*
      LINK3res NFSPROC3_LINK(LINK3args) = 15;

      struct LINK3args {
           nfs_fh3     file;
           diropargs3  link;
      };

      struct LINK3resok {
           post_op_attr   file_attributes;
           wcc_data       linkdir_wcc;
      };

      struct LINK3resfail {
           post_op_attr   file_attributes;
           wcc_data       linkdir_wcc;
      };

      union LINK3res switch (nfsstat3 status) {
      case NFS3_OK:
           LINK3resok    resok;
      default:
           LINK3resfail  resfail;
      };
*/

#[allow(non_camel_case_types)]
#[derive(Debug, Default)]
struct LINK3args {
    file: nfs::nfs_fh3,
    link: nfs::diropargs3,
}
XDRStruct!(LINK3args, file, link);

pub async fn nfsproc3_link(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut args = LINK3args::default();
    args.deserialize(input)?;

    debug!("nfsproc3_link({:?}, {:?}) ", xid, args);

    // Get the file to link to
    let fileid = context.vfs.fh_to_id(&args.file);
    if let Err(stat) = fileid {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::post_op_attr::Void.serialize(output)?;
        nfs::wcc_data::default().serialize(output)?;
        return Ok(());
    }
    let fileid = fileid.unwrap();

    // Get file attributes
    let file_attr = match context.vfs.getattr(fileid).await {
        Ok(v) => v,
        Err(stat) => {
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::post_op_attr::Void.serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
            return Ok(());
        }
    };

    // Cannot hard link directories
    if matches!(file_attr.ftype, nfs::ftype3::NF3DIR) {
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ISDIR.serialize(output)?;
        nfs::post_op_attr::attributes(file_attr).serialize(output)?;
        nfs::wcc_data::default().serialize(output)?;
        return Ok(());
    }

    // Get the directory where the link will be created
    let dirid = context.vfs.fh_to_id(&args.link.dir);
    if let Err(stat) = dirid {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::post_op_attr::attributes(file_attr).serialize(output)?;
        nfs::wcc_data::default().serialize(output)?;
        return Ok(());
    }
    let dirid = dirid.unwrap();

    // Get directory attributes
    let dir_attr = match context.vfs.getattr(dirid).await {
        Ok(v) => v,
        Err(stat) => {
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::post_op_attr::attributes(file_attr).serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
            return Ok(());
        }
    };

    let pre_dir_attr = nfs::pre_op_attr::attributes(nfs::wcc_attr {
        size: dir_attr.size,
        mtime: dir_attr.mtime,
        ctime: dir_attr.ctime,
    });

    let creds = permissions::credentials(&context.auth);
    let dir_stats = permissions::stats(&dir_attr);
    // Check write and execute permission on directory
    if !access::may_modify_directory(&dir_stats, &creds) {
        debug!(
            "link permission denied for uid={} on directory",
            context.auth.uid
        );
        let post_dir_attr = match context.vfs.getattr(dirid).await {
            Ok(v) => nfs::post_op_attr::attributes(v),
            Err(_) => nfs::post_op_attr::Void,
        };
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ACCES.serialize(output)?;
        nfs::post_op_attr::attributes(file_attr).serialize(output)?;
        nfs::wcc_data {
            before: pre_dir_attr,
            after: post_dir_attr,
        }
        .serialize(output)?;
        return Ok(());
    }

    // Create the link
    let res = context.vfs.link(fileid, dirid, &args.link.name).await;

    // Re-read dir attributes for post op attr
    let post_dir_attr = match context.vfs.getattr(dirid).await {
        Ok(v) => nfs::post_op_attr::attributes(v),
        Err(_) => nfs::post_op_attr::Void,
    };
    let wcc_res = nfs::wcc_data {
        before: pre_dir_attr,
        after: post_dir_attr,
    };

    match res {
        Ok(new_attr) => {
            debug!("link success {:?}", xid);
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            nfs::post_op_attr::attributes(new_attr).serialize(output)?;
            wcc_res.serialize(output)?;
        }
        Err(e) => {
            error!("link error {:?} --> {:?}", xid, e);
            // Re-read file attributes
            let post_file_attr = match context.vfs.getattr(fileid).await {
                Ok(v) => nfs::post_op_attr::attributes(v),
                Err(_) => nfs::post_op_attr::Void,
            };
            make_success_reply(xid).serialize(output)?;
            e.serialize(output)?;
            post_file_attr.serialize(output)?;
            wcc_res.serialize(output)?;
        }
    }

    Ok(())
}

/*

 READLINK3res NFSPROC3_READLINK(READLINK3args) = 5;

 struct READLINK3args {
      nfs_fh3  symlink;
 };

 struct READLINK3resok {
      post_op_attr   symlink_attributes;
      nfspath3       data;
 };

 struct READLINK3resfail {
      post_op_attr   symlink_attributes;
 };

 union READLINK3res switch (nfsstat3 status) {
 case NFS3_OK:
      READLINK3resok   resok;
 default:
      READLINK3resfail resfail;
 };
*/
pub async fn nfsproc3_readlink(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut handle = nfs::nfs_fh3::default();
    handle.deserialize(input)?;
    debug!("nfsproc3_readlink({:?},{:?}) ", xid, handle);

    let id = context.vfs.fh_to_id(&handle);
    // fail if unable to convert file handle
    if let Err(stat) = id {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        return Ok(());
    }
    let id = id.unwrap();
    // if the id does not exist, we fail
    let symlink_attr = match context.vfs.getattr(id).await {
        Ok(v) => nfs::post_op_attr::attributes(v),
        Err(stat) => {
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::post_op_attr::Void.serialize(output)?;
            return Ok(());
        }
    };
    match context.vfs.readlink(id).await {
        Ok(path) => {
            debug!(" {:?} --> {:?}", xid, path);
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            symlink_attr.serialize(output)?;
            path.serialize(output)?;
        }
        Err(stat) => {
            // failed to read link
            // retry with failure and the post_op_attr
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            symlink_attr.serialize(output)?;
        }
    }
    Ok(())
}

/*
 MKNOD3res NFSPROC3_MKNOD(MKNOD3args) = 11;

 struct devicedata3 {
    sattr3     dev_attributes;
    specdata3  spec;
 };

 union mknoddata3 switch (ftype3 type) {
 case NF3CHR:
 case NF3BLK:
    devicedata3  device;
 case NF3SOCK:
 case NF3FIFO:
    sattr3       pipe_attributes;
 default:
    void;
 };

 struct MKNOD3args {
    diropargs3   where;
    mknoddata3   what;
 };

 struct MKNOD3resok {
    post_op_fh3   obj;
    post_op_attr  obj_attributes;
    wcc_data      dir_wcc;
 };

 struct MKNOD3resfail {
    wcc_data      dir_wcc;
 };

 union MKNOD3res switch (nfsstat3 status) {
 case NFS3_OK:
    MKNOD3resok   resok;
 default:
    MKNOD3resfail resfail;
 };
*/
pub async fn nfsproc3_mknod(
    xid: u32,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    // Read diropargs3 (where to create)
    let mut dirops = nfs::diropargs3::default();
    dirops.deserialize(input)?;

    // Read ftype3 (what type of node to create)
    let mut ftype: u32 = 0;
    ftype.deserialize(input)?;
    let ftype = nfs::ftype3::from_u32(ftype).unwrap_or(nfs::ftype3::NF3REG);

    // Read type-specific data
    let (attr, rdev) = match ftype {
        nfs::ftype3::NF3CHR | nfs::ftype3::NF3BLK => {
            // devicedata3: sattr3 + specdata3
            let mut attr = nfs::sattr3::default();
            attr.deserialize(input)?;
            let mut rdev = nfs::specdata3::default();
            rdev.deserialize(input)?;
            (attr, rdev)
        }
        nfs::ftype3::NF3SOCK | nfs::ftype3::NF3FIFO => {
            // pipe_attributes: just sattr3
            let mut attr = nfs::sattr3::default();
            attr.deserialize(input)?;
            (attr, nfs::specdata3::default())
        }
        _ => {
            // Invalid type for mknod
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3ERR_BADTYPE.serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
            return Ok(());
        }
    };

    debug!("nfsproc3_mknod({:?}, {:?}, {:?}) ", xid, dirops, ftype);

    // find the directory we are supposed to create the node in
    let dirid = context.vfs.fh_to_id(&dirops.dir);
    if let Err(stat) = dirid {
        make_success_reply(xid).serialize(output)?;
        stat.serialize(output)?;
        nfs::wcc_data::default().serialize(output)?;
        error!("Directory does not exist");
        return Ok(());
    }
    let dirid = dirid.unwrap();

    // get the directory attributes before the operation
    let dir_attr = match context.vfs.getattr(dirid).await {
        Ok(v) => v,
        Err(stat) => {
            error!("Cannot stat directory");
            make_success_reply(xid).serialize(output)?;
            stat.serialize(output)?;
            nfs::wcc_data::default().serialize(output)?;
            return Ok(());
        }
    };

    let pre_dir_attr = nfs::pre_op_attr::attributes(nfs::wcc_attr {
        size: dir_attr.size,
        mtime: dir_attr.mtime,
        ctime: dir_attr.ctime,
    });

    let creds = permissions::credentials(&context.auth);
    let dir_stats = permissions::stats(&dir_attr);
    // Check write and execute permission on parent directory
    if !access::may_modify_directory(&dir_stats, &creds) {
        debug!(
            "mknod permission denied for uid={} on directory",
            context.auth.uid
        );
        let post_dir_attr = match context.vfs.getattr(dirid).await {
            Ok(v) => nfs::post_op_attr::attributes(v),
            Err(_) => nfs::post_op_attr::Void,
        };
        make_success_reply(xid).serialize(output)?;
        nfs::nfsstat3::NFS3ERR_ACCES.serialize(output)?;
        nfs::wcc_data {
            before: pre_dir_attr,
            after: post_dir_attr,
        }
        .serialize(output)?;
        return Ok(());
    }

    let res = context
        .vfs
        .mknod(dirid, &dirops.name, ftype, attr, rdev, &context.auth)
        .await;

    // Re-read dir attributes for post op attr
    let post_dir_attr = match context.vfs.getattr(dirid).await {
        Ok(v) => nfs::post_op_attr::attributes(v),
        Err(_) => nfs::post_op_attr::Void,
    };
    let wcc_res = nfs::wcc_data {
        before: pre_dir_attr,
        after: post_dir_attr,
    };

    match res {
        Ok((fid, fattr)) => {
            debug!("mknod success --> {:?}, {:?}", fid, fattr);
            make_success_reply(xid).serialize(output)?;
            nfs::nfsstat3::NFS3_OK.serialize(output)?;
            let fh = context.vfs.id_to_fh(fid);
            nfs::post_op_fh3::handle(fh).serialize(output)?;
            nfs::post_op_attr::attributes(fattr).serialize(output)?;
            wcc_res.serialize(output)?;
        }
        Err(e) => {
            debug!("mknod error --> {:?}", e);
            make_success_reply(xid).serialize(output)?;
            e.serialize(output)?;
            wcc_res.serialize(output)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::AgentNFS;
    use crate::server::rpc::{accept_body, accepted_reply, reply_body, rpc_body, rpc_msg};
    use crate::server::transaction_tracker::{TransactionTracker, DEFAULT_REPLY_CACHE_CAPACITY};
    use crate::server::vfs::NFSFileSystem;
    use agentfs_core::fs::MAX_NAME_LEN;
    use agentfs_core::{AgentFS as AgentSdk, AgentFSOptions, FileSystem};
    use std::io::Cursor;
    use std::path::Path;
    use std::sync::Arc;

    const TEST_UID: u32 = 1000;
    const TEST_GID: u32 = 1000;

    fn make_auth(uid: u32, gid: u32, gids: Vec<u32>) -> auth_unix {
        auth_unix {
            stamp: 0,
            machinename: Vec::new(),
            uid,
            gid,
            gids,
        }
    }

    fn make_attr(mode: u32, uid: u32, gid: u32, ftype: nfs::ftype3) -> nfs::fattr3 {
        nfs::fattr3 {
            ftype,
            mode,
            nlink: 1,
            uid,
            gid,
            size: 0,
            used: 0,
            rdev: nfs::specdata3::default(),
            fsid: 0,
            fileid: 2,
            atime: nfs::nfstime3::default(),
            mtime: nfs::nfstime3::default(),
            ctime: nfs::nfstime3::default(),
        }
    }

    async fn test_context() -> (RPCContext, agentfs_core::fs::AgentFS) {
        let agent = AgentSdk::open(AgentFSOptions::ephemeral())
            .await
            .expect("open ephemeral AgentFS");
        test_context_from_agent(agent).await
    }

    async fn test_context_with_db_path(db_path: &Path) -> (RPCContext, agentfs_core::fs::AgentFS) {
        let agent = AgentSdk::open(AgentFSOptions::with_path(
            db_path.to_str().expect("test DB path is UTF-8"),
        ))
        .await
        .expect("open file-backed AgentFS");
        test_context_from_agent(agent).await
    }

    async fn test_context_from_agent(agent: AgentSdk) -> (RPCContext, agentfs_core::fs::AgentFS) {
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
            client_addr: "127.0.0.1:1".to_string(),
            auth: auth_unix {
                stamp: 0,
                machinename: b"test".to_vec(),
                uid: TEST_UID,
                gid: TEST_GID,
                gids: vec![TEST_GID],
            },
            vfs,
            export_name: Arc::new("/".to_string()),
            transaction_tracker: Arc::new(TransactionTracker::new(DEFAULT_REPLY_CACHE_CAPACITY)),
        };
        (context, fs)
    }

    fn force_long_write_batcher_window() {
        std::env::set_var("AGENTFS_FUSE_WRITEBACK", "1");
        std::env::set_var("AGENTFS_OVERLAY_READS", "1");
        std::env::set_var("AGENTFS_BATCH_MS", "60000");
        std::env::set_var("AGENTFS_BATCH_BYTES", "1048576");
        std::env::set_var("AGENTFS_BATCH_GLOBAL_BYTES", "10485760");
    }

    fn parse_rpc_success(cursor: &mut Cursor<Vec<u8>>) {
        let mut reply = rpc_msg::default();
        reply.deserialize(cursor).expect("deserialize RPC reply");
        match reply.body {
            rpc_body::REPLY(reply_body::MSG_ACCEPTED(accepted_reply {
                reply_data: accept_body::SUCCESS,
                ..
            })) => {}
            other => panic!("unexpected RPC reply: {other:?}"),
        }
    }

    fn parse_nfs_status(cursor: &mut Cursor<Vec<u8>>) -> nfs::nfsstat3 {
        let mut status = nfs::nfsstat3::NFS3_OK;
        status
            .deserialize(cursor)
            .expect("deserialize NFS response status");
        status
    }

    fn serialize_create_readonly_args(root_fh: nfs::nfs_fh3) -> Vec<u8> {
        let mut input = Vec::new();
        let mut cursor = Cursor::new(&mut input);
        nfs::diropargs3 {
            dir: root_fh,
            name: b"loose-object".as_slice().into(),
        }
        .serialize(&mut cursor)
        .expect("serialize CREATE dirops");
        createmode3::UNCHECKED
            .serialize(&mut cursor)
            .expect("serialize CREATE mode");
        nfs::sattr3 {
            mode: nfs::set_mode3::mode(0o444),
            ..Default::default()
        }
        .serialize(&mut cursor)
        .expect("serialize CREATE attrs");
        input
    }

    fn serialize_write_args(file: nfs::nfs_fh3, data: &[u8]) -> Vec<u8> {
        let mut input = Vec::new();
        let mut cursor = Cursor::new(&mut input);
        WRITE3args {
            file,
            offset: 0,
            count: data.len() as u32,
            stable: stable_how::FILE_SYNC as u32,
            data: data.to_vec(),
        }
        .serialize(&mut cursor)
        .expect("serialize WRITE args");
        input
    }

    fn serialize_readdirplus_args(root_fh: nfs::nfs_fh3) -> Vec<u8> {
        serialize_readdirplus_page_args(root_fh, 0, 8192, 8192)
    }

    fn serialize_readdir_args(
        root_fh: nfs::nfs_fh3,
        cookie: nfs::cookie3,
        dircount: u32,
    ) -> Vec<u8> {
        let mut input = Vec::new();
        let mut cursor = Cursor::new(&mut input);
        READDIR3args {
            dir: root_fh,
            cookie,
            cookieverf: nfs::cookieverf3::default(),
            dircount,
        }
        .serialize(&mut cursor)
        .expect("serialize READDIR args");
        input
    }

    fn serialize_readdirplus_page_args(
        root_fh: nfs::nfs_fh3,
        cookie: nfs::cookie3,
        dircount: u32,
        maxcount: u32,
    ) -> Vec<u8> {
        let mut input = Vec::new();
        let mut cursor = Cursor::new(&mut input);
        READDIRPLUS3args {
            dir: root_fh,
            cookie,
            cookieverf: nfs::cookieverf3::default(),
            dircount,
            maxcount,
        }
        .serialize(&mut cursor)
        .expect("serialize READDIRPLUS args");
        input
    }

    fn serialize_getattr_args(file: nfs::nfs_fh3) -> Vec<u8> {
        let mut input = Vec::new();
        let mut cursor = Cursor::new(&mut input);
        file.serialize(&mut cursor).expect("serialize GETATTR fh");
        input
    }

    fn serialize_setattr_size_args(file: nfs::nfs_fh3, size: u64) -> Vec<u8> {
        serialize_setattr_size_args_with_guard(file, size, sattrguard3::Void)
    }

    fn serialize_setattr_size_args_with_guard(
        file: nfs::nfs_fh3,
        size: u64,
        guard: sattrguard3,
    ) -> Vec<u8> {
        let mut input = Vec::new();
        let mut cursor = Cursor::new(&mut input);
        SETATTR3args {
            object: file,
            new_attribute: nfs::sattr3 {
                size: nfs::set_size3::size(size),
                ..Default::default()
            },
            guard,
        }
        .serialize(&mut cursor)
        .expect("serialize SETATTR size args");
        input
    }

    #[derive(Clone, Copy)]
    enum ReaddirProcedure {
        Readdir,
        ReaddirPlus,
    }

    struct ReaddirPage {
        names: Vec<Vec<u8>>,
        cookies: Vec<nfs::cookie3>,
        eof: bool,
    }

    async fn create_numbered_files(context: &RPCContext, count: usize) -> Vec<Vec<u8>> {
        let mut names = Vec::with_capacity(count);
        for index in 0..count {
            let name = format!("entry-{index:04}").into_bytes();
            context
                .vfs
                .create(
                    1,
                    &name.clone().into(),
                    nfs::sattr3::default(),
                    &context.auth,
                )
                .await
                .expect("numbered file create succeeds");
            names.push(name);
        }
        names
    }

    async fn read_readdir_page(
        context: &RPCContext,
        procedure: ReaddirProcedure,
        cookie: nfs::cookie3,
    ) -> ReaddirPage {
        let root = context.vfs.id_to_fh(1);
        let mut input = Cursor::new(match procedure {
            ReaddirProcedure::Readdir => serialize_readdir_args(root, cookie, 512),
            ReaddirProcedure::ReaddirPlus => {
                serialize_readdirplus_page_args(root, cookie, 512, 1200)
            }
        });
        let mut output = Vec::new();
        match procedure {
            ReaddirProcedure::Readdir => nfsproc3_readdir(6, &mut input, &mut output, context)
                .await
                .expect("READDIR handler"),
            ReaddirProcedure::ReaddirPlus => {
                nfsproc3_readdirplus(7, &mut input, &mut output, context)
                    .await
                    .expect("READDIRPLUS handler")
            }
        }

        let mut cursor = Cursor::new(output);
        parse_rpc_success(&mut cursor);
        let status = parse_nfs_status(&mut cursor);
        assert!(matches!(status, nfs::nfsstat3::NFS3_OK));
        let mut dir_attr = nfs::post_op_attr::Void;
        dir_attr
            .deserialize(&mut cursor)
            .expect("deserialize dir attrs");
        let mut cookieverf = nfs::cookieverf3::default();
        cookieverf
            .deserialize(&mut cursor)
            .expect("deserialize cookie verifier");

        let mut names = Vec::new();
        let mut cookies = Vec::new();
        loop {
            let mut has_entry = false;
            has_entry
                .deserialize(&mut cursor)
                .expect("deserialize entry presence");
            if !has_entry {
                break;
            }
            match procedure {
                ReaddirProcedure::Readdir => {
                    let mut entry = entry3::default();
                    entry.deserialize(&mut cursor).expect("deserialize entry");
                    names.push(entry.name.to_vec());
                    cookies.push(entry.cookie);
                }
                ReaddirProcedure::ReaddirPlus => {
                    let mut entry = entryplus3::default();
                    entry
                        .deserialize(&mut cursor)
                        .expect("deserialize entryplus");
                    names.push(entry.name.to_vec());
                    cookies.push(entry.cookie);
                }
            }
        }
        let mut eof = false;
        eof.deserialize(&mut cursor).expect("deserialize eof");
        assert_eq!(
            names.len(),
            cookies.len(),
            "every returned entry carries a pagination cookie"
        );
        assert!(
            !names.is_empty() || eof,
            "a non-final page must make progress with at least one entry"
        );
        ReaddirPage {
            names,
            cookies,
            eof,
        }
    }

    async fn collect_readdir_pages(
        context: &RPCContext,
        procedure: ReaddirProcedure,
    ) -> Vec<ReaddirPage> {
        let mut pages = Vec::new();
        let mut cookie = 0;
        for _ in 0..64 {
            let page = read_readdir_page(context, procedure, cookie).await;
            if let Some(next_cookie) = page.cookies.last().copied() {
                assert!(
                    page.eof || next_cookie != cookie,
                    "pagination did not honor cookie {cookie}: page repeated the same final cookie"
                );
                cookie = next_cookie;
            }
            let eof = page.eof;
            pages.push(page);
            if eof {
                return pages;
            }
        }
        panic!("READDIR pagination did not reach EOF within the safety bound");
    }

    #[test]
    fn nfs_access_cases_match_shared_access() {
        let requested = permissions::ACCESS3_READ
            | permissions::ACCESS3_LOOKUP
            | permissions::ACCESS3_MODIFY
            | permissions::ACCESS3_EXTEND
            | permissions::ACCESS3_DELETE
            | permissions::ACCESS3_EXECUTE;
        let cases = [
            (
                "nfs owner regular rwx",
                make_auth(1000, 1000, vec![]),
                make_attr(0o700, 1000, 2000, nfs::ftype3::NF3REG),
            ),
            (
                "nfs auxiliary group read",
                make_auth(1000, 1000, vec![2000]),
                make_attr(0o040, 3000, 2000, nfs::ftype3::NF3REG),
            ),
            (
                "nfs owner directory rwx",
                make_auth(1000, 1000, vec![]),
                make_attr(0o700, 1000, 2000, nfs::ftype3::NF3DIR),
            ),
            (
                "nfs root regular mode zero",
                make_auth(0, 0, vec![]),
                make_attr(0o000, 1000, 1000, nfs::ftype3::NF3REG),
            ),
        ];

        println!("adapter=nfs authority=semantics::access");
        let mut mismatches = 0usize;
        for (name, auth, attr) in cases {
            let stats = permissions::stats(&attr);
            let creds = permissions::credentials(&auth);
            let mut shared = 0;
            if access::may_read(&stats, &creds) {
                shared |= permissions::ACCESS3_READ;
            }
            if stats.is_directory() && access::may_search(&stats, &creds) {
                shared |= permissions::ACCESS3_LOOKUP;
            }
            if access::may_write(&stats, &creds) {
                shared |= permissions::ACCESS3_MODIFY | permissions::ACCESS3_EXTEND;
                if stats.is_directory() {
                    shared |= permissions::ACCESS3_DELETE;
                }
            }
            if !stats.is_directory() && access::may_search(&stats, &creds) {
                shared |= permissions::ACCESS3_EXECUTE;
            }

            let nfs = permissions::compute_access(&auth, &attr, requested);
            println!("{name}: nfs={nfs:#04x} access={shared:#04x}");
            if nfs != shared {
                mismatches += 1;
            }
        }
        println!("adapter=nfs access_conformance mismatches={mismatches}");
        assert_eq!(mismatches, 0);
    }

    #[tokio::test]
    async fn readdir_and_readdirplus_honor_cookies_without_duplicates_or_skips() {
        let (context, _fs) = test_context().await;
        let expected = create_numbered_files(&context, 24).await;

        for procedure in [ReaddirProcedure::Readdir, ReaddirProcedure::ReaddirPlus] {
            let pages = collect_readdir_pages(&context, procedure).await;
            assert!(
                pages.len() > 1,
                "test must force pagination across multiple pages"
            );

            for (index, page) in pages.iter().enumerate() {
                assert_eq!(
                    page.eof,
                    index + 1 == pages.len(),
                    "eof must be false until the final page"
                );
            }

            let mut flat = Vec::new();
            for page in &pages {
                flat.extend(page.names.iter().cloned());
            }
            let mut unique = flat.clone();
            unique.sort();
            unique.dedup();

            println!(
                "procedure={} pages={} total={} unique={} first_entries={:?}",
                match procedure {
                    ReaddirProcedure::Readdir => "READDIR",
                    ReaddirProcedure::ReaddirPlus => "READDIRPLUS",
                },
                pages.len(),
                flat.len(),
                unique.len(),
                pages
                    .iter()
                    .map(|page| String::from_utf8_lossy(&page.names[0]).into_owned())
                    .collect::<Vec<_>>()
            );

            assert_eq!(
                unique.len(),
                flat.len(),
                "pagination returned duplicate entries"
            );
            assert_eq!(unique, expected, "pagination skipped or reordered entries");
        }
    }

    #[tokio::test]
    async fn pathconf_name_max_and_create_limits_match_sdk_enforcement() {
        let (context, _fs) = test_context().await;
        let mut input = Cursor::new({
            let mut bytes = Vec::new();
            context
                .vfs
                .id_to_fh(1)
                .serialize(&mut bytes)
                .expect("serialize PATHCONF handle");
            bytes
        });
        let mut output = Vec::new();

        nfsproc3_pathconf(8, &mut input, &mut output, &context)
            .await
            .expect("PATHCONF handler");

        let mut cursor = Cursor::new(output);
        parse_rpc_success(&mut cursor);
        let status = parse_nfs_status(&mut cursor);
        assert!(matches!(status, nfs::nfsstat3::NFS3_OK));
        let mut pathconf = PATHCONF3resok::default();
        pathconf
            .deserialize(&mut cursor)
            .expect("deserialize PATHCONF result");
        println!(
            "pathconf.name_max={} sdk.MAX_NAME_LEN={}",
            pathconf.name_max, MAX_NAME_LEN
        );
        assert_eq!(pathconf.name_max, MAX_NAME_LEN as u32);

        let ok_name = vec![b'a'; MAX_NAME_LEN];
        context
            .vfs
            .create(1, &ok_name.into(), nfs::sattr3::default(), &context.auth)
            .await
            .expect("255-byte filename create succeeds");

        let too_long_name = vec![b'b'; MAX_NAME_LEN + 1];
        let err = context
            .vfs
            .create(
                1,
                &too_long_name.into(),
                nfs::sattr3::default(),
                &context.auth,
            )
            .await
            .expect_err("256-byte filename create fails");
        assert!(matches!(err, nfs::nfsstat3::NFS3ERR_NAMETOOLONG));

        let fsinfo = context.vfs.fsinfo(1).await.expect("FSINFO succeeds");
        assert_eq!(
            fsinfo.rtpref,
            1024 * 1024,
            "rtpref should match the 1MiB macOS mount rsize preference"
        );
    }

    async fn create_readonly_file(context: &RPCContext) -> nfs::nfs_fh3 {
        let mut input = Cursor::new(serialize_create_readonly_args(context.vfs.id_to_fh(1)));
        let mut output = Vec::new();
        nfsproc3_create(1, &mut input, &mut output, context)
            .await
            .expect("CREATE handler");

        let mut cursor = Cursor::new(output);
        parse_rpc_success(&mut cursor);
        let status = parse_nfs_status(&mut cursor);
        assert!(matches!(status, nfs::nfsstat3::NFS3_OK));

        let mut fh = nfs::post_op_fh3::default();
        fh.deserialize(&mut cursor).expect("deserialize CREATE fh");
        match fh {
            nfs::post_op_fh3::handle(fh) => fh,
            nfs::post_op_fh3::Void => panic!("CREATE did not return a file handle"),
        }
    }

    async fn write_status(context: &RPCContext, file: nfs::nfs_fh3, data: &[u8]) -> nfs::nfsstat3 {
        let mut input = Cursor::new(serialize_write_args(file, data));
        let mut output = Vec::new();
        nfsproc3_write(2, &mut input, &mut output, context)
            .await
            .expect("WRITE handler");

        let mut cursor = Cursor::new(output);
        parse_rpc_success(&mut cursor);
        parse_nfs_status(&mut cursor)
    }

    async fn write_file_sync_result(
        context: &RPCContext,
        file: nfs::nfs_fh3,
        data: &[u8],
    ) -> (nfs::nfsstat3, Option<WRITE3resok>) {
        let mut input = Cursor::new(serialize_write_args(file, data));
        let mut output = Vec::new();
        nfsproc3_write(2, &mut input, &mut output, context)
            .await
            .expect("WRITE handler");

        let mut cursor = Cursor::new(output);
        parse_rpc_success(&mut cursor);
        let status = parse_nfs_status(&mut cursor);
        if matches!(status, nfs::nfsstat3::NFS3_OK) {
            let mut resok = WRITE3resok::default();
            resok
                .deserialize(&mut cursor)
                .expect("deserialize WRITE3resok");
            (status, Some(resok))
        } else {
            (status, None)
        }
    }

    async fn readdirplus_handle_for_name(context: &RPCContext, name: &[u8]) -> nfs::nfs_fh3 {
        let mut input = Cursor::new(serialize_readdirplus_args(context.vfs.id_to_fh(1)));
        let mut output = Vec::new();
        nfsproc3_readdirplus(5, &mut input, &mut output, context)
            .await
            .expect("READDIRPLUS handler");

        let mut cursor = Cursor::new(output);
        parse_rpc_success(&mut cursor);
        let status = parse_nfs_status(&mut cursor);
        assert!(matches!(status, nfs::nfsstat3::NFS3_OK));
        let mut dir_attr = nfs::post_op_attr::Void;
        dir_attr
            .deserialize(&mut cursor)
            .expect("deserialize READDIRPLUS dir attrs");
        let mut cookieverf = nfs::cookieverf3::default();
        cookieverf
            .deserialize(&mut cursor)
            .expect("deserialize READDIRPLUS cookie verifier");

        loop {
            let mut has_entry = false;
            has_entry
                .deserialize(&mut cursor)
                .expect("deserialize READDIRPLUS entry presence");
            if !has_entry {
                panic!("READDIRPLUS did not return requested entry {name:?}");
            }
            let mut entry = entryplus3::default();
            entry
                .deserialize(&mut cursor)
                .expect("deserialize READDIRPLUS entry");
            if entry.name.as_slice() == name {
                return match entry.name_handle {
                    nfs::post_op_fh3::handle(fh) => fh,
                    nfs::post_op_fh3::Void => panic!("READDIRPLUS entry omitted file handle"),
                };
            }
        }
    }

    async fn getattr_result(
        context: &RPCContext,
        file: nfs::nfs_fh3,
    ) -> (nfs::nfsstat3, Option<nfs::fattr3>) {
        let mut input = Cursor::new(serialize_getattr_args(file));
        let mut output = Vec::new();
        nfsproc3_getattr(4, &mut input, &mut output, context)
            .await
            .expect("GETATTR handler");

        let mut cursor = Cursor::new(output);
        parse_rpc_success(&mut cursor);
        let status = parse_nfs_status(&mut cursor);
        if matches!(status, nfs::nfsstat3::NFS3_OK) {
            let mut attr = nfs::fattr3::default();
            attr.deserialize(&mut cursor)
                .expect("deserialize GETATTR fattr");
            (status, Some(attr))
        } else {
            (status, None)
        }
    }

    async fn setattr_size_status(
        context: &RPCContext,
        file: nfs::nfs_fh3,
        size: u64,
    ) -> nfs::nfsstat3 {
        let mut input = Cursor::new(serialize_setattr_size_args(file, size));
        let mut output = Vec::new();
        nfsproc3_setattr(3, &mut input, &mut output, context)
            .await
            .expect("SETATTR handler");

        let mut cursor = Cursor::new(output);
        parse_rpc_success(&mut cursor);
        parse_nfs_status(&mut cursor)
    }

    async fn setattr_size_status_with_guard(
        context: &RPCContext,
        file: nfs::nfs_fh3,
        size: u64,
        guard: sattrguard3,
    ) -> nfs::nfsstat3 {
        let mut input = Cursor::new(serialize_setattr_size_args_with_guard(file, size, guard));
        let mut output = Vec::new();
        nfsproc3_setattr(3, &mut input, &mut output, context)
            .await
            .expect("SETATTR handler");

        let mut cursor = Cursor::new(output);
        parse_rpc_success(&mut cursor);
        parse_nfs_status(&mut cursor)
    }

    #[test]
    fn write_reply_committed_field_derives_from_ack_durability() {
        assert_eq!(
            stable_how_for_ack_durability(AckDurability::Committed),
            stable_how::FILE_SYNC
        );
        assert_eq!(
            stable_how_for_ack_durability(AckDurability::Volatile),
            stable_how::UNSTABLE
        );
        assert_eq!(
            ack_durability_for_write_request(stable_how::FILE_SYNC as u32),
            AckDurability::Committed
        );
        assert_eq!(
            ack_durability_for_write_request(stable_how::UNSTABLE as u32),
            AckDurability::Committed,
            "current NFS policy commits every WRITE until COMMIT support opts in"
        );
    }

    async fn read_file(fs: &agentfs_core::fs::AgentFS, name: &str, len: u64) -> Vec<u8> {
        let stats = fs
            .lookup(1, name)
            .await
            .expect("lookup file")
            .expect("file exists");
        let file = FileSystem::open(fs, stats.ino, libc::O_RDONLY)
            .await
            .expect("open file");
        file.pread(0, len).await.expect("read file")
    }

    #[tokio::test]
    async fn file_sync_write_reply_survives_abort_before_batch_timer() {
        force_long_write_batcher_window();
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("nfs-file-sync.db");
        let (context, fs) = test_context_with_db_path(&db_path).await;
        let created_fh = create_readonly_file(&context).await;
        let payload = b"FILE_SYNC bytes must survive immediate abort";

        let (status, resok) = write_file_sync_result(&context, created_fh, payload).await;

        assert!(matches!(status, nfs::nfsstat3::NFS3_OK));
        let resok = resok.expect("WRITE should return success payload");
        assert!(matches!(resok.committed, stable_how::FILE_SYNC));
        println!(
            "FILE_SYNC WRITE replied NFS3_OK count={} committed={:?}; aborting server before batch timer",
            resok.count, resok.committed
        );

        drop(context);
        drop(fs);

        let reopened = AgentSdk::open(AgentFSOptions::with_path(
            db_path.to_str().expect("test DB path is UTF-8"),
        ))
        .await
        .expect("reopen AgentFS after simulated server abort");
        let persisted = read_file(&reopened.fs, "loose-object", payload.len() as u64).await;
        println!(
            "reopened DB persisted {} bytes after FILE_SYNC abort",
            persisted.len()
        );
        assert_eq!(
            persisted, payload,
            "NFS FILE_SYNC reply must not be sent before bytes are durable"
        );
    }

    #[tokio::test]
    async fn write_wcc_and_getattr_are_coherent_with_acknowledged_data() {
        force_long_write_batcher_window();
        let (context, _fs) = test_context().await;
        let created_fh = create_readonly_file(&context).await;
        let payload = b"coherent attrs after ack";

        let (status, resok) = write_file_sync_result(&context, created_fh.clone(), payload).await;

        assert!(matches!(status, nfs::nfsstat3::NFS3_OK));
        let resok = resok.expect("WRITE should return success payload");
        assert_eq!(resok.count, payload.len() as u32);
        assert!(matches!(resok.committed, stable_how::FILE_SYNC));
        let after = match resok.file_wcc.after {
            nfs::post_op_attr::attributes(attr) => attr,
            nfs::post_op_attr::Void => panic!("WRITE wcc omitted post-op attrs"),
        };
        assert_eq!(after.size, payload.len() as u64);
        match resok.file_wcc.before {
            nfs::pre_op_attr::attributes(before) => {
                assert!(
                    after.mtime.seconds > before.mtime.seconds
                        || (after.mtime.seconds == before.mtime.seconds
                            && after.mtime.nseconds >= before.mtime.nseconds),
                    "WRITE wcc mtime must be non-decreasing"
                );
            }
            nfs::pre_op_attr::Void => panic!("WRITE wcc omitted pre-op attrs"),
        }

        let (getattr_status, getattr_attr) = getattr_result(&context, created_fh.clone()).await;
        assert!(matches!(getattr_status, nfs::nfsstat3::NFS3_OK));
        let getattr_attr = getattr_attr.expect("GETATTR should return attrs");
        assert_eq!(getattr_attr.size, payload.len() as u64);
        assert!(
            getattr_attr.mtime.seconds > after.mtime.seconds
                || (getattr_attr.mtime.seconds == after.mtime.seconds
                    && getattr_attr.mtime.nseconds >= after.mtime.nseconds),
            "GETATTR mtime must not move backwards after WRITE wcc"
        );

        let truncated_size = 7;
        let setattr_status =
            setattr_size_status(&context, created_fh.clone(), truncated_size).await;
        assert!(matches!(setattr_status, nfs::nfsstat3::NFS3_OK));
        let (getattr_status, getattr_attr) = getattr_result(&context, created_fh).await;
        assert!(matches!(getattr_status, nfs::nfsstat3::NFS3_OK));
        assert_eq!(
            getattr_attr
                .expect("GETATTR after SETATTR should return attrs")
                .size,
            truncated_size
        );
    }

    #[tokio::test]
    async fn create_authorized_handle_can_write_after_readonly_final_mode() {
        let (context, fs) = test_context().await;
        let created_fh = create_readonly_file(&context).await;

        let status = write_status(&context, created_fh, b"data").await;

        assert!(matches!(status, nfs::nfsstat3::NFS3_OK));
        assert_eq!(read_file(&fs, "loose-object", 4).await, b"data");
    }

    #[tokio::test]
    async fn readdirplus_refreshed_handle_can_write_after_readonly_final_mode() {
        let (context, fs) = test_context().await;
        let created_fh = create_readonly_file(&context).await;
        let created_id = context
            .vfs
            .fh_to_id(&created_fh)
            .expect("created handle resolves");

        let readdirplus_fh = readdirplus_handle_for_name(&context, b"loose-object").await;

        assert!(context
            .vfs
            .fh_has_write_authority(&readdirplus_fh, created_id));
        let status = write_status(&context, readdirplus_fh.clone(), b"data").await;
        assert!(matches!(status, nfs::nfsstat3::NFS3_OK));
        let (getattr_status, getattr_attr) = getattr_result(&context, readdirplus_fh).await;
        assert!(matches!(getattr_status, nfs::nfsstat3::NFS3_OK));
        assert_eq!(getattr_attr.expect("attrs").mode, 0o444);
        assert_eq!(read_file(&fs, "loose-object", 4).await, b"data");
    }

    #[tokio::test]
    async fn fresh_lookup_handle_without_write_permission_stays_denied() {
        let (context, fs) = test_context().await;
        let created_fh = create_readonly_file(&context).await;
        let created_id = context
            .vfs
            .fh_to_id(&created_fh)
            .expect("created handle resolves");
        let plain_fh = context.vfs.id_to_fh(created_id);

        let status = write_status(&context, plain_fh, b"nope").await;

        assert!(matches!(status, nfs::nfsstat3::NFS3ERR_ACCES));
        assert_eq!(read_file(&fs, "loose-object", 4).await, b"");
    }

    #[tokio::test]
    async fn create_authorized_handle_can_truncate_after_readonly_final_mode() {
        let (context, fs) = test_context().await;
        let created_fh = create_readonly_file(&context).await;
        assert!(matches!(
            write_status(&context, created_fh.clone(), b"abcdef").await,
            nfs::nfsstat3::NFS3_OK
        ));

        let status = setattr_size_status(&context, created_fh, 3).await;

        assert!(matches!(status, nfs::nfsstat3::NFS3_OK));
        assert_eq!(read_file(&fs, "loose-object", 8).await, b"abc");
    }

    #[tokio::test]
    async fn fresh_lookup_handle_without_write_permission_cannot_truncate() {
        let (context, fs) = test_context().await;
        let created_fh = create_readonly_file(&context).await;
        assert!(matches!(
            write_status(&context, created_fh.clone(), b"abcdef").await,
            nfs::nfsstat3::NFS3_OK
        ));
        let created_id = context
            .vfs
            .fh_to_id(&created_fh)
            .expect("created handle resolves");
        let plain_fh = context.vfs.id_to_fh(created_id);

        let status = setattr_size_status(&context, plain_fh, 3).await;

        assert!(matches!(status, nfs::nfsstat3::NFS3ERR_ACCES));
        assert_eq!(read_file(&fs, "loose-object", 8).await, b"abcdef");
    }

    #[tokio::test]
    async fn setattr_guard_mismatch_does_not_truncate() {
        let (context, fs) = test_context().await;
        let created_fh = create_readonly_file(&context).await;
        assert!(matches!(
            write_status(&context, created_fh.clone(), b"abcdef").await,
            nfs::nfsstat3::NFS3_OK
        ));

        let stale_guard = sattrguard3::obj_ctime(nfs::nfstime3 {
            seconds: 0,
            nseconds: 0,
        });
        let status = setattr_size_status_with_guard(&context, created_fh, 3, stale_guard).await;

        assert!(matches!(status, nfs::nfsstat3::NFS3ERR_NOT_SYNC));
        assert_eq!(read_file(&fs, "loose-object", 8).await, b"abcdef");
    }
}
