extern crate libc;

use std::cell::RefCell;
use std::mem;
use std::net;
use std::ptr;
use std::rc::Rc;

type SysResult<T> = Result<T, i32>;

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

fn sa_to_raw(sa: &net::SocketAddrV4) -> libc::sockaddr_in {
    let ip = sa.ip().octets();
    libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: sa.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: (ip[3] as u32) << 24
                | (ip[2] as u32) << 16
                | (ip[1] as u32) << 8
                | (ip[0] as u32),
        },
        ..unsafe { mem::zeroed() }
    }
}

fn sa6_to_raw(sa: &net::SocketAddrV6) -> libc::sockaddr_in6 {
    let mut inaddr: libc::in6_addr = unsafe { mem::zeroed() };
    inaddr.s6_addr = sa.ip().octets();
    libc::sockaddr_in6 {
        sin6_family: libc::AF_INET6 as libc::sa_family_t,
        sin6_port: sa.port().to_be(),
        sin6_flowinfo: sa.flowinfo(),
        sin6_addr: inaddr,
        sin6_scope_id: sa.scope_id(),
    }
}

fn connect_tcp(addr: &net::SocketAddr) -> SysResult<i32> {
    let fd = syscall!(libc::socket(
        match *addr {
            net::SocketAddr::V4(_) => libc::AF_INET,
            net::SocketAddr::V6(_) => libc::AF_INET6,
        },
        libc::SOCK_STREAM | libc::SOCK_NONBLOCK,
        0,
    ))?;
    let r = match addr {
        &net::SocketAddr::V4(sa) => {
            let sin = sa_to_raw(&sa);
            syscall!(libc::connect(
                fd,
                &sin as *const _ as *const _,
                mem::size_of_val(&sin) as libc::socklen_t
            ))
        }
        &net::SocketAddr::V6(sa) => {
            let sin = sa6_to_raw(&sa);
            syscall!(libc::connect(
                fd,
                &sin as *const _ as *const _,
                mem::size_of_val(&sin) as libc::socklen_t
            ))
        }
    };
    if let Err(e) = r {
        if e != libc::EINPROGRESS {
            unsafe { libc::close(fd) };
            return Err(e);
        }
    }
    Ok(fd)
}

fn listen_tcp(addr: &net::SocketAddr) -> SysResult<i32> {
    let fd = syscall!(libc::socket(
        match *addr {
            net::SocketAddr::V4(_) => libc::AF_INET,
            net::SocketAddr::V6(_) => libc::AF_INET6,
        },
        libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
        0,
    ))?;
    let r = match addr {
        &net::SocketAddr::V4(sa) => {
            let sin = sa_to_raw(&sa);
            syscall!(libc::bind(
                fd,
                &sin as *const _ as *const _,
                mem::size_of_val(&sin) as libc::socklen_t
            ))
        }
        &net::SocketAddr::V6(sa) => {
            let sin = sa6_to_raw(&sa);
            syscall!(libc::bind(
                fd,
                &sin as *const _ as *const _,
                mem::size_of_val(&sin) as libc::socklen_t
            ))
        }
    };
    if let Err(e) = r {
        unsafe { libc::close(fd) };
        return Err(e);
    }
    let r = syscall!(libc::listen(fd, libc::SOMAXCONN));
    if let Err(e) = r {
        unsafe { libc::close(fd) };
        Err(e)
    } else {
        Ok(fd)
    }
}

static mut EPOLL_FD_: i32 = 0;
static EPOLL_FD: &i32 = unsafe { &EPOLL_FD_ };

fn epoll_add(fd: i32, rw: i32, data: u64) -> SysResult<i32> {
    let mut events = libc::EPOLLET;
    if rw & 1 != 0 {
        events |= libc::EPOLLIN;
    }
    if rw & 2 != 0 {
        events |= libc::EPOLLOUT;
    }
    syscall!(libc::epoll_ctl(
        *EPOLL_FD,
        libc::EPOLL_CTL_ADD,
        fd,
        &libc::epoll_event {
            events: events as u32,
            u64: data
        } as *const _ as *mut _,
    ))
}

fn epoll_del(fd: i32) -> SysResult<i32> {
    syscall!(libc::epoll_ctl(
        *EPOLL_FD,
        libc::EPOLL_CTL_DEL,
        fd,
        ptr::null_mut(),
    ))
}

static mut PIPE_SIZE_: isize = 0;
static PIPE_SIZE: &isize = unsafe { &PIPE_SIZE_ };

struct IoBuf {
    pfd: [i32; 2],
    buffered: isize,
}

impl IoBuf {
    fn new() -> IoBuf {
        let mut pfd = [0; 2];
        syscall!(libc::pipe(pfd.as_mut_ptr())).unwrap();
        IoBuf {
            pfd: pfd,
            buffered: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.buffered == 0
    }

    fn splice_in(&mut self, fd: i32) -> SysResult<bool> {
        let max_size = *PIPE_SIZE;
        while self.buffered < max_size {
            let r = syscall!(libc::splice(
                fd,
                ptr::null_mut(),
                self.pfd[1],
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
                return Ok(true);
            }
            self.buffered += n;
        }
        Ok(false)
    }

    fn splice_out(&mut self, fd: i32) -> SysResult<()> {
        while self.buffered > 0 {
            let r = syscall!(libc::splice(
                self.pfd[0],
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

impl Drop for IoBuf {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.pfd[0]);
            libc::close(self.pfd[1]);
        }
    }
}

struct Context {
    bad: bool,
    client_fd: i32,
    backend_fd: i32,
    in_buf: IoBuf,
    out_buf: IoBuf,
    in_pd: u64,
    out_pd: u64,
}

impl Context {
    fn new(client_fd: i32, backend_fd: i32) -> Context {
        Context {
            bad: false,
            client_fd,
            backend_fd,
            in_buf: IoBuf::new(),
            out_buf: IoBuf::new(),
            in_pd: 0,
            out_pd: 0,
        }
    }

    fn copy(buf: &mut IoBuf, from_fd: i32, to_fd: i32) -> SysResult<()> {
        let eof = buf.splice_in(from_fd)?;
        if !buf.is_empty() {
            buf.splice_out(to_fd)?;
        }
        if eof && buf.is_empty() {
            Err(0)
        } else {
            Ok(())
        }
    }

    fn copy_from(&mut self) -> SysResult<()> {
        if self.bad {
            Err(0)
        } else {
            Context::copy(&mut self.in_buf, self.client_fd, self.backend_fd)
        }
    }

    fn copy_to(&mut self) -> SysResult<()> {
        if self.bad {
            Err(0)
        } else {
            Context::copy(&mut self.out_buf, self.backend_fd, self.client_fd)
        }
    }

    fn shutdown(&mut self) {
        if !self.bad {
            epoll_del(self.client_fd).unwrap();
            epoll_del(self.backend_fd).unwrap();
            mem::drop(unsafe { Box::from_raw(self.in_pd as *mut PollDesp) });
            mem::drop(unsafe { Box::from_raw(self.out_pd as *mut PollDesp) });
            self.bad = true
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        println!("Context drop: {}+{}", self.client_fd, self.backend_fd);
        unsafe {
            libc::close(self.client_fd);
            libc::close(self.backend_fd);
        }
    }
}

struct PollDesp {
    who: i32,
    ctx: Rc<RefCell<Context>>,
}

impl Drop for PollDesp {
    fn drop(&mut self) {
        println!("PollDesp drop: {}", self.who);
    }
}

fn handle_client(client_fd: i32) {
    let res = connect_tcp(&"127.0.0.1:9527".parse().unwrap());
    let backend_fd = match res {
        Ok(fd) => fd,
        Err(e) => {
            println!("connect backend failed: {}", e);
            unsafe { libc::close(client_fd) };
            return;
        }
    };
    println!(
        "associate client_fd {} backend_fd {}",
        client_fd, backend_fd
    );
    let ctx = Rc::new(RefCell::new(Context::new(client_fd, backend_fd)));
    {
        let in_pd = Box::into_raw(Box::new(PollDesp {
            who: 0,
            ctx: ctx.clone(),
        })) as u64;
        let out_pd = Box::into_raw(Box::new(PollDesp {
            who: 1,
            ctx: ctx.clone(),
        })) as u64;
        let mut ctx = ctx.borrow_mut();
        ctx.in_pd = in_pd;
        ctx.out_pd = out_pd;
        epoll_add(client_fd, 3, in_pd).unwrap();
        epoll_add(backend_fd, 3, out_pd).unwrap();
    }
}

fn main() {
    {
        let mut pfd = [0; 2];
        syscall!(libc::pipe(pfd.as_mut_ptr())).unwrap();
        syscall!(libc::fcntl(pfd[0], libc::F_GETPIPE_SZ))
            .map(|n| {
                unsafe {
                    PIPE_SIZE_ = n as isize;
                };
                ()
            }).unwrap();
        unsafe {
            libc::close(pfd[0]);
            libc::close(pfd[1]);
        }

        println!("pipe size: {}", *PIPE_SIZE);
    }

    syscall!(libc::epoll_create1(0))
        .map(|fd| unsafe {
            EPOLL_FD_ = fd;
        }).unwrap();

    let listen_fd = listen_tcp(&"0.0.0.0:5262".parse().unwrap()).unwrap();
    epoll_add(listen_fd, 1, 0).unwrap();

    println!("listen ok");

    let mut events: [libc::epoll_event; 64] = unsafe { mem::zeroed() };
    loop {
        println!("polling events");
        let res = syscall!(libc::epoll_wait(
            *EPOLL_FD,
            events.as_mut_ptr(),
            events.len() as i32,
            -1
        ));
        let n = match res {
            Ok(n) => n,
            Err(e) => {
                if e == libc::EINTR {
                    continue;
                }
                panic!("epoll_wait failed: {}", e);
            }
        };
        println!("epoll {} events raised", n);
        let mut defer_free = Vec::new();
        for i in 0..n as usize {
            if events[i].u64 == 0 {
                loop {
                    match syscall!(libc::accept4(
                        listen_fd,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        libc::SOCK_NONBLOCK,
                    )) {
                        Ok(fd) => {
                            println!("accept client_fd: {}", fd);
                            handle_client(fd);
                        }
                        Err(e) => {
                            if e == libc::EAGAIN {
                                break;
                            } else {
                                panic!("accept failed: {}", e);
                            }
                        }
                    };
                }
                continue;
            }
            let pd = unsafe { Box::from_raw(events[i].u64 as *mut PollDesp) };
            let mut free = false;
            if events[i].events & (libc::EPOLLIN | libc::EPOLLRDHUP | libc::EPOLLERR) as u32 != 0 {
                let res = if pd.who == 0 {
                    pd.ctx.borrow_mut().copy_from()
                } else {
                    pd.ctx.borrow_mut().copy_to()
                };
                if let Err(e) = res {
                    println!("copy data failed on IN: {}", e);
                    free = true;
                }
            }
            if events[i].events & (libc::EPOLLOUT | libc::EPOLLERR | libc::EPOLLHUP) as u32 != 0 {
                let res = if pd.who == 1 {
                    pd.ctx.borrow_mut().copy_from()
                } else {
                    pd.ctx.borrow_mut().copy_to()
                };
                if let Err(e) = res {
                    println!("copy data failed on OUT: {}", e);
                    free = true;
                }
            }
            if free {
                defer_free.push(pd.ctx.clone());
            }
            mem::forget(pd);
        }
        for v in defer_free {
            let mut ctx = v.borrow_mut();
            ctx.shutdown();
        }
    }
}
