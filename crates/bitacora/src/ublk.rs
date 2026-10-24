//! ublk frontend (contract section 11), only with the `ublk` feature.
//!
//! One queue of depth 64 served by one thread. Each request is decoded and
//! run by `frontend.rs`; this module only talks to libublk. The device is
//! created with `UBLK_F_USER_RECOVERY | UBLK_F_USER_RECOVERY_REISSUE`: when
//! the process dies the kernel keeps `/dev/ublkbN` and queues its requests,
//! and `recover` attaches a new process that reissues them. Reissuing a
//! write is safe because a repeated write of the same data is idempotent in
//! this engine.

use std::io::{self, Write};
use std::sync::Arc;

use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::io::{BufDescList, UblkDev, UblkIOCtx, UblkQueue};
use libublk::{BufDesc, UblkError, UblkFlags, UblkIORes, sys};

use crate::format::BLOCK_SIZE;
use crate::frontend::{self, Request};
use crate::{Error, Result, Volume, server};

/// Requests in flight in the single queue.
pub const QUEUE_DEPTH: u16 = 64;

/// `logical_bs_shift`: 4096-byte logical blocks.
const BLOCK_SHIFT: u8 = 12;

fn ublk_error(e: UblkError) -> Error {
    Error::Io(io::Error::other(format!("ublk: {e}")))
}

/// Serves `vol` as a ublk block device until `SIGTERM` or `SIGINT`, which
/// stops the device; then closes the volume and deletes the device. With
/// `recover`, takes over the device `DEV_ID` left by a killed process
/// instead of creating one. Prints the device id on standard output once
/// the device is live.
///
/// The caller must have called `server::block_signals` before creating any
/// thread, the volume's included.
pub fn serve(vol: Arc<Volume>, recover: Option<u32>) -> Result<()> {
    let (id, dev_flags) = match recover {
        Some(dev_id) => {
            let id = i32::try_from(dev_id)
                .map_err(|_| Error::InvalidArgument(format!("invalid device id: {dev_id}")))?;
            let rc = UblkCtrl::new_simple(id)
                .and_then(|c| c.start_user_recover())
                .map_err(ublk_error)?;
            if rc < 0 {
                return Err(Error::Io(io::Error::from_raw_os_error(-rc)));
            }
            (id, UblkFlags::UBLK_DEV_F_RECOVER_DEV)
        }
        None => (-1, UblkFlags::UBLK_DEV_F_ADD_DEV),
    };
    let ctrl = UblkCtrlBuilder::default()
        .name("bitacora")
        .id(id)
        .nr_queues(1_u16)
        .depth(QUEUE_DEPTH)
        .ctrl_flags((sys::UBLK_F_USER_RECOVERY | sys::UBLK_F_USER_RECOVERY_REISSUE) as u64)
        .dev_flags(dev_flags)
        .build()
        .map_err(ublk_error)?;
    let dev_id = ctrl.dev_info().dev_id;

    // STOP_DEV from another control handle: the queue thread then sees its
    // queue go down and `run_target` returns.
    server::on_signal(move || {
        if let Ok(c) = UblkCtrl::new_simple(dev_id as i32) {
            let _ = c.kill_dev();
        }
    })?;

    let size = vol.volume_size();
    let tgt_init = move |dev: &mut UblkDev| {
        dev.tgt.dev_size = size;
        dev.tgt.params = sys::ublk_params {
            types: sys::UBLK_PARAM_TYPE_BASIC,
            basic: sys::ublk_param_basic {
                attrs: sys::UBLK_ATTR_VOLATILE_CACHE | sys::UBLK_ATTR_FUA,
                logical_bs_shift: BLOCK_SHIFT,
                physical_bs_shift: BLOCK_SHIFT,
                io_opt_shift: BLOCK_SHIFT,
                io_min_shift: BLOCK_SHIFT,
                max_sectors: dev.dev_info.max_io_buf_bytes >> 9,
                dev_sectors: size >> 9,
                ..Default::default()
            },
            ..Default::default()
        };
        Ok(())
    };
    let queue_vol = vol.clone();
    let q_fn = move |qid: u16, dev: &UblkDev| serve_queue(qid, dev, &queue_vol);
    let started = move |c: &UblkCtrl| {
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "{}", c.dev_info().dev_id);
        let _ = out.flush();
    };
    let served = ctrl.run_target(tgt_init, q_fn, started).map_err(ublk_error);

    let closed = vol.close();
    let deleted = ctrl.del_dev().map_err(ublk_error);
    served?;
    closed?;
    deleted?;
    Ok(())
}

/// Serves queue `qid` until it goes down: one request at a time, each
/// completed with its byte count or a negative errno.
fn serve_queue(qid: u16, dev: &UblkDev, vol: &Volume) {
    let bufs = dev.alloc_queue_io_bufs();
    let queue = match UblkQueue::new(qid, dev)
        .and_then(|q| q.submit_fetch_commands_unified(BufDescList::Slices(Some(&bufs))))
    {
        Ok(q) => q,
        Err(e) => {
            eprintln!("bitacora: ublk queue {qid}: {e}");
            return;
        }
    };
    queue.wait_and_handle_io(|q: &UblkQueue, tag: u16, _io: &UblkIOCtx| {
        let iod = q.get_iod(tag);
        let req = Request::from_ublk(iod.op_flags, iod.start_sector, iod.nr_sectors);
        let buf = &bufs[usize::from(tag)];
        // SAFETY: the buffer of `tag` belongs to this request until it is
        // completed below; the kernel does not touch it meanwhile and this
        // thread is the only one serving the queue.
        let data = unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len()) };
        let res = match frontend::execute(vol, &req, data) {
            Ok(bytes) => i32::try_from(bytes).unwrap_or(-libc::EINVAL),
            Err(e) => {
                if !matches!(e, frontend::FrontendError::Unsupported(_)) {
                    eprintln!("bitacora: ublk: {e}");
                }
                -e.errno()
            }
        };
        if let Err(e) =
            q.complete_io_cmd_unified(tag, BufDesc::Slice(buf.as_slice()), Ok(UblkIORes::Result(res)))
        {
            eprintln!("bitacora: ublk: completing tag {tag}: {e}");
        }
    });
}

// The device's logical block is the engine block.
const _: () = assert!(BLOCK_SIZE == 1 << BLOCK_SHIFT);
