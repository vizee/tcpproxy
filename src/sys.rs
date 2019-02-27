use std::ptr;

use libc;

pub type SysResult<T> = Result<T, i32>;

macro_rules! syscall {
    ($e: expr) => {{
        let r = unsafe { $e };
        if r < 0 {
            Err(unsafe { *libc::__errno_location() })
        } else {
            Ok(r)
        }
    }};
}

static mut PIPE_SIZE: isize = 0;

pub struct PipeBuf {
    buffered: isize,
    pfd_r: i32,
    pfd_w: i32,
}

impl PipeBuf {
    pub fn new() -> PipeBuf {
        let mut pfd = [0; 2];
        syscall!(libc::pipe(pfd.as_mut_ptr())).unwrap();
        println!("PipeBuf::new: {}+{}", pfd[0], pfd[1]);
        PipeBuf {
            buffered: 0,
            pfd_r: pfd[0],
            pfd_w: pfd[1],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buffered == 0
    }

    pub fn splice_in(&mut self, fd: i32) -> SysResult<bool> {
        let max_size = unsafe { PIPE_SIZE };
        while self.buffered < max_size {
            let r = syscall!(libc::splice(
                fd,
                ptr::null_mut(),
                self.pfd_w,
                ptr::null_mut(),
                (max_size - self.buffered) as usize,
                libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK
            ));
            let n = match r {
                Ok(n) => n,
                Err(e) => {
                    if e == libc::EAGAIN {
                        break;
                    }
                    return Err(e);
                }
            };
            if n == 0 {
                if self.buffered == 0 {
                    return Ok(true);
                }
                break;
            }
            self.buffered += n;
        }
        Ok(false)
    }

    pub fn splice_out(&mut self, fd: i32) -> SysResult<()> {
        while self.buffered > 0 {
            let r = syscall!(libc::splice(
                self.pfd_r,
                ptr::null_mut(),
                fd,
                ptr::null_mut(),
                self.buffered as usize,
                libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK
            ));
            let n = match r {
                Ok(n) => n,
                Err(e) => {
                    if e == libc::EAGAIN {
                        break;
                    }
                    return Err(e);
                }
            };
            self.buffered -= n;
        }
        Ok(())
    }
}

impl Drop for PipeBuf {
    fn drop(&mut self) {
        println!("PipeBuf::drop: {}+{}", self.pfd_r, self.pfd_w);
        unsafe {
            libc::close(self.pfd_r);
            libc::close(self.pfd_w);
        }
    }
}

pub fn init() -> SysResult<()> {
    let mut pfd = [0; 2];
    syscall!(libc::pipe(pfd.as_mut_ptr()))?;
    let res = syscall!(libc::fcntl(pfd[0], libc::F_GETPIPE_SZ))
        .map(|n| unsafe { PIPE_SIZE = n as isize });
    unsafe {
        libc::close(pfd[0]);
        libc::close(pfd[1]);
    }
    res
}
