#![feature(const_string_new)]

use std::env;
use std::mem;
use std::ptr;
use std::rc::Rc;

use crate::sys::{PipeBuf, SysResult};

#[macro_use]
mod sys;
mod net;

struct Global {
    epfd: i32,
    backend: String,
}

static mut GLOBAL: Global = Global {
    epfd: 0,
    backend: String::new(),
};

fn global() -> &'static Global {
    return unsafe { &GLOBAL };
}

fn epoll_add(fd: i32, events: i32, data: u64) -> SysResult<i32> {
    syscall!(libc::epoll_ctl(
        global().epfd,
        libc::EPOLL_CTL_ADD,
        fd,
        &libc::epoll_event {
            events: (libc::EPOLLET | events) as u32,
            u64: data
        } as *const _ as *mut _,
    ))
}

fn epoll_del(fd: i32) -> SysResult<i32> {
    syscall!(libc::epoll_ctl(
        global().epfd,
        libc::EPOLL_CTL_DEL,
        fd,
        ptr::null_mut(),
    ))
}

struct Context {
    bad: bool,
    client_fd: i32,
    backend_fd: i32,
    in_buf: PipeBuf,
    out_buf: PipeBuf,
    in_pd: u64,
    out_pd: u64,
}

impl Context {
    fn new(client_fd: i32, backend_fd: i32) -> Context {
        println!("Context::new: {}+{}", client_fd, backend_fd);
        Context {
            bad: false,
            client_fd,
            backend_fd,
            in_buf: PipeBuf::new(),
            out_buf: PipeBuf::new(),
            in_pd: 0,
            out_pd: 0,
        }
    }

    fn copy(buf: &mut PipeBuf, from_fd: i32, to_fd: i32) -> SysResult<()> {
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    Client,
    Backend,
}

struct PollDesp {
    who: Owner,
    ctx: Rc<Context>,
}

#[inline]
fn mutable<T, F, R>(x: &Rc<T>, f: F) -> R
    where
        T: ?Sized,
        F: Fn(&mut T) -> R,
{
    f(unsafe { &mut *(&**x as *const _ as *mut T) })
}

fn handle_client(client_fd: i32) {
    let ba = net::resolve_first(&global().backend, libc::AF_INET, libc::SOCK_STREAM, false)
        .expect("bad address");
    let res = net::connect_tcp(&ba);
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
    let ctx = Rc::new(Context::new(client_fd, backend_fd));
    let in_pd = Box::into_raw(Box::new(PollDesp {
        who: Owner::Client,
        ctx: ctx.clone(),
    })) as u64;
    let out_pd = Box::into_raw(Box::new(PollDesp {
        who: Owner::Backend,
        ctx: ctx.clone(),
    })) as u64;
    mutable(&ctx, |ctx| {
        ctx.in_pd = in_pd;
        ctx.out_pd = out_pd;
    });
    epoll_add(client_fd, libc::EPOLLIN | libc::EPOLLOUT, in_pd).unwrap();
    epoll_add(backend_fd, libc::EPOLLIN | libc::EPOLLOUT, out_pd).unwrap();
}

struct Config {
    listen: String,
    dst: String,
}

fn parse_args() -> Result<Config, &'static str> {
    let mut config = Config {
        listen: ":8080".to_string(),
        dst: "127.0.0.1:9090".to_string(),
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-l" => {
                if let Some(listen) = args.next() {
                    config.listen = listen;
                } else {
                    return Err("missing argument for -l");
                }
            }
            "-d" => {
                if let Some(dst) = args.next() {
                    config.dst = dst;
                } else {
                    return Err("missing argument for -d");
                }
            }
            _ => return Err("tcpproxy [-l <listen>] [-d <backend>]"),
        }
    }
    Ok(config)
}

fn main() {
    let config = parse_args()
        .expect("invalid option");
    unsafe {
        GLOBAL.backend = config.dst;
    }

    sys::init().unwrap();

    syscall!(libc::epoll_create1(0))
        .map(|fd| unsafe {
            GLOBAL.epfd = fd;
        })
        .expect("epoll_create failed");

    println!("listen {}", config.listen);
    let la = net::resolve_first(&config.listen, libc::AF_INET, libc::SOCK_STREAM, true)
        .expect("bad address");
    let listen_fd = net::listen_tcp(&la)
        .expect("listen failed");
    epoll_add(listen_fd, libc::EPOLLIN, 0).unwrap();

    let mut events: [libc::epoll_event; 64] = unsafe { mem::zeroed() };
    loop {
        let res = syscall!(libc::epoll_wait(
            global().epfd,
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
        let mut unused = Vec::new();
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
            let pd = unsafe { &*(events[i].u64 as *mut PollDesp) };
            let mut free = false;
            if events[i].events & (libc::EPOLLIN | libc::EPOLLERR | libc::EPOLLRDHUP) as u32 != 0 {
                let res = if pd.who == Owner::Client {
                    mutable(&pd.ctx, |ctx| ctx.copy_from())
                } else {
                    mutable(&pd.ctx, |ctx| ctx.copy_to())
                };
                if let Err(e) = res {
                    println!("copy data failed on IN: {}", e);
                    free = true;
                }
            }
            if events[i].events & (libc::EPOLLOUT | libc::EPOLLERR | libc::EPOLLHUP) as u32 != 0 {
                let res = if pd.who == Owner::Backend {
                    mutable(&pd.ctx, |ctx| ctx.copy_from())
                } else {
                    mutable(&pd.ctx, |ctx| ctx.copy_to())
                };
                if let Err(e) = res {
                    println!("copy data failed on OUT: {}", e);
                    free = true;
                }
            }
            if free {
                unused.push(pd.ctx.clone());
            }
        }
        for ctx in unused {
            mutable(&ctx, |ctx| ctx.shutdown());
        }
    }
}
