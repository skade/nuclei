use std::io;
use iou::{IoUring, SubmissionQueue, CompletionQueue, SubmissionQueueEvent, CompletionQueueEvent, Registrar};
use lever::sync::prelude::*;
use std::collections::HashMap;
use futures::channel::oneshot;
use pin_utils::unsafe_pinned;
use std::future::Future;
use core::mem::MaybeUninit;
use std::os::unix::io::{AsRawFd, RawFd, FromRawFd};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use std::sync::atomic::{AtomicU64, Ordering, AtomicBool};

macro_rules! syscall {
    ($fn:ident $args:tt) => {{
        let res = unsafe { libc::$fn $args };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

///////////////////
///////////////////

use socket2::SockAddr;
use std::os::unix::net::{SocketAddr as UnixSocketAddr};
use std::mem;
use crate::Proactor;
use once_cell::sync::Lazy;

fn max_len() -> usize {
    // The maximum read limit on most posix-like systems is `SSIZE_MAX`,
    // with the man page quoting that if the count of bytes to read is
    // greater than `SSIZE_MAX` the result is "unspecified".
    //
    // On macOS, however, apparently the 64-bit libc is either buggy or
    // intentionally showing odd behavior by rejecting any read with a size
    // larger than or equal to INT_MAX. To handle both of these the read
    // size is capped on both platforms.
    if cfg!(target_os = "macos") {
        <libc::c_int>::max_value() as usize - 1
    } else {
        <libc::ssize_t>::max_value() as usize
    }
}

pub(crate) fn shim_recv_from<A: AsRawFd>(fd: A, buf: &mut [u8], flags: libc::c_int) -> io::Result<(usize, SockAddr)> {
    let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let mut addrlen = mem::size_of_val(&storage) as libc::socklen_t;

    let n = syscall!(recvfrom(
            fd.as_raw_fd() as _,
            buf.as_mut_ptr() as *mut libc::c_void,
            std::cmp::min(buf.len(), max_len()),
            flags,
            &mut storage as *mut _ as *mut _,
            &mut addrlen,
        ))?;
    let addr = unsafe { SockAddr::from_raw_parts(&storage as *const _ as *const _, addrlen) };
    Ok((n as usize, addr))
}

struct FakeUnixSocketAddr {
    addr: libc::sockaddr_un,
    len: libc::socklen_t,
}

pub(crate) fn shim_to_af_unix(sockaddr: &SockAddr) -> io::Result<UnixSocketAddr> {
    let addr = unsafe { &*(sockaddr.as_ptr() as *const libc::sockaddr_un) };
    if addr.sun_family != libc::AF_UNIX as libc::sa_family_t {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "socket is not AF_UNIX type",
        ));
    }

    let mut len = sockaddr.len();
    let abst_sock_ident: libc::c_char = unsafe {
        std::slice::from_raw_parts(
            &addr.sun_path as *const _ as *const u8,
            mem::size_of::<libc::c_char>()
        )
    }[1] as libc::c_char;

    match (len, abst_sock_ident) {
        // NOTE: (vertexclique): If it is abstract socket, sa is greater than
        // sa_family_t, in that case assign len as the sa_family_t size.
        // https://man7.org/linux/man-pages/man7/unix.7.html
        (sa, 0) if sa != 0 && sa > mem::size_of::<libc::sa_family_t>() as libc::socklen_t => {
            len = mem::size_of::<libc::sa_family_t>() as libc::socklen_t;
        },
        // If unnamed socket, then addr is always zero,
        // assign the offset reserved difference as length.
        (0, _) => {
            let base = &addr as *const _ as usize;
            let path = &addr.sun_path as *const _ as usize;
            let sun_path_offset = path - base;
            len = sun_path_offset as libc::socklen_t;
        },

        // Discard rest, they are not special.
        (_, _) => {}
    }

    let addr: UnixSocketAddr = unsafe {
        let mut init = MaybeUninit::<libc::sockaddr_un>::zeroed();
        // Safety: `*sockaddr` and `&init` are not overlapping and `*sockaddr`
        // points to valid memory.
        std::ptr::copy_nonoverlapping(
            sockaddr.as_ptr(),
            &mut init as *mut _ as *mut _,
            len as usize
        );

        // Safety: We've written the init addr above.
        std::mem::transmute(FakeUnixSocketAddr {
            addr: init.assume_init(),
            len: len as _,
        })
    };

    Ok(addr)
}

///////////////////
//// uring impl
///////////////////

// const MANUAL_TIMEOUT: u64 = u64::max_value() - 1;
const MANUAL_TIMEOUT: u64 = -2 as _;
const QUEUE_LEN: u32 = 1 << 8;
// const QUEUE_LEN: u32 = 1 << 0;

pub struct SysProactor {
    sq: TTas<SubmissionQueue<'static>>,
    cq: TTas<CompletionQueue<'static>>,
    submitters: TTas<HashMap<u64, oneshot::Sender<i32>>>,
    submitter_id: AtomicU64,
    waker: AtomicBool,
}

pub type RingTypes = (SubmissionQueue<'static>, CompletionQueue<'static>, Registrar<'static>);

static mut IO_URING: Option<IoUring> = None;

impl SysProactor {
    // fn init_ring() -> mut IoUring {
    //     static mut IO_URING: Lazy<IoUring> = Lazy::new(|| {
    //         IoUring::new(QUEUE_LEN).expect("uring can't be initialized")
    //     });
    //
    //     &*IO_URING
    // }

    pub(crate) fn new() -> io::Result<SysProactor> {
        unsafe {
            IO_URING = Some(iou::IoUring::new(QUEUE_LEN).expect("uring can't be initialized"));
            let (sq, cq, _) = IO_URING.as_mut().unwrap().queues();

            Ok(SysProactor {
                sq: TTas::new(sq),
                cq: TTas::new(cq),
                submitters: TTas::new(HashMap::default()),
                submitter_id: AtomicU64::new(1),
                waker: AtomicBool::default()
            })
        }
    }

    fn submitter<T>(&self, sq: &mut SubmissionQueue<'_>, mut ring_sub: impl FnMut(&mut SubmissionQueueEvent<'_>) -> T) -> Option<T> {
        // dbg!("SUBMITTER");
        let mut sqe = match sq.next_sqe() {
            Some(sqe) => sqe,
            None => {
                if sq.submit().is_err() {
                    return None;
                }
                sq.next_sqe()?
            }
        };

        let mut id = self.submitter_id.fetch_add(1, Ordering::Relaxed);
        if id == MANUAL_TIMEOUT {
            id = self.submitter_id.fetch_add(1, Ordering::Relaxed);
        }
        sqe.set_user_data(id);

        Some(ring_sub(&mut sqe))
    }


    pub(crate) fn register_io(&self, mut io_submit: impl FnMut(&mut SubmissionQueueEvent<'_>)) -> io::Result<CompletionChan> {
        dbg!("REGISTER IO");
        let sub_comp = {
            dbg!("RIO ENTER");
            let mut sq = self.sq.lock();
            dbg!("RIO EXIT");

            let cc = self.submitter(&mut sq, |sqe| {
                // dbg!("SUBMITTER");
                let id = sqe.user_data();
                let (tx, rx) = oneshot::channel();

                dbg!("SUBMITTER", id);
                io_submit(sqe);

                {
                    let mut subguard = self.submitters.lock();
                    subguard.insert(id, tx);
                    dbg!("INSERTED", id);
                }

                CompletionChan { rx }
            }).map(|c| {
                let submitted_io_evcount = sq.submit().unwrap();
                dbg!(submitted_io_evcount);

                c
            });

            cc
        };

        // dbg!(sub_comp.is_none());

        sub_comp.ok_or(io::Error::from(io::ErrorKind::WouldBlock))
    }

    pub(crate) fn wake(&self) -> io::Result<()> {
        // dbg!("WAKE");
        // {
        //     let mut sq = self.sq.lock();
        //
        //     let res = submitter(&mut sq, |sqe| {
        //         unsafe {
        //             let sqep = sqe.raw_mut();
        //             sqep.user_data = MANUAL_TIMEOUT;
        //             uring_sys::io_uring_prep_timeout_remove(sqep, MANUAL_TIMEOUT, 0);
        //         }
        //     }).map(|c| {
        //         sq.submit().unwrap();
        //         c
        //     });
        //
        //     res.ok_or(io::Error::from(io::ErrorKind::WouldBlock))?
        // }
        Ok(())
    }

    pub(crate) fn wait(&self, max_event_size: usize, duration: Option<Duration>) -> io::Result<usize> {
        dbg!("WAIT ENTER");
        let mut cq = self.cq.lock();
        dbg!("CQ ACK");

        let mut acquired = 0;

        dbg!("before wait cqe");
        while let Ok(cqe) = cq.wait_for_cqe() {
            dbg!("non cqe");
            let mut ready = cq.ready() as usize + 1;

            self.cqe_completion(acquired, &cqe);
            ready -= 1;

            while let Some(cqe) = cq.peek_for_cqe() {
                if ready == 0 {
                    ready = cq.ready() as usize + 1;
                }

                self.cqe_completion(acquired, &cqe);
                ready -= 1;
            }
        }

        Ok(acquired)
    }

    fn cqe_completion(&self, mut acquired: usize, cqe: &CompletionQueueEvent) {
        if cqe.is_timeout() {
            dbg!("MAYBE CQE WAS TIMEOUT");
            return;
        }

        let udata = cqe.user_data();
        let res = cqe.raw_result() as i32;
        if udata == MANUAL_TIMEOUT {
            return;
        }

        acquired += 1;
        dbg!("ACQUIRED", udata);

        self.submitters.lock()
            .remove(&udata)
            .map(|s| {
                dbg!("CALLBACK SENT");
                s.send(res)
            });
        dbg!("SENT");
    }
}

pub(crate) struct CompletionChan {
    rx: oneshot::Receiver<i32>,
}

impl CompletionChan {
    unsafe_pinned!(rx: oneshot::Receiver<i32>);
}

impl Future for CompletionChan {
    type Output = io::Result<i32>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.rx()
            .poll(cx)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "sender has been cancelled"))
    }
}
