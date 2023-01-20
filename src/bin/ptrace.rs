#![allow(dead_code)]

use nix::{
    libc,
    sys::{
        pthread::{pthread_self, Pthread},
        ptrace,
        wait::waitpid,
    },
    unistd::{fork, gettid, read, write, ForkResult, Pid},
};

use std::{
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        mpsc::sync_channel,
        Mutex,
    },
    thread::{yield_now, JoinHandle},
};

fn main() {
    let available_parallelism = 2;

    let _workers: Vec<_> = (0..available_parallelism)
        .map(|_| Worker::spawn(busy::work))
        .collect();

    Tracer::spawn();

    std::thread::sleep(std::time::Duration::from_secs(1));

    TRACE_PLS.store(true, Ordering::SeqCst);

    std::thread::sleep(std::time::Duration::from_secs(5));
}

#[derive(Debug)]
struct Tracer {
    join_handle: JoinHandle<()>,
}

static TRACE_PLS: AtomicBool = AtomicBool::new(false);
static TRACER_PID: AtomicI32 = AtomicI32::new(0);

impl Tracer {
    pub fn spawn() {
        std::thread::spawn(move || {
            let tid = gettid().as_raw();
            TRACER_PID.store(tid, Ordering::SeqCst);

            loop {
                if TRACE_PLS
                    .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    let workers = WORKERS.lock().unwrap();
                    let (on_pause_read_fd, on_pause_write_fd) = nix::unistd::pipe().unwrap();
                    let (on_resume_read_fd, on_resume_write_fd) = nix::unistd::pipe().unwrap();
                    match unsafe { fork() } {
                        Ok(ForkResult::Parent { child, .. }) => {
                            // wait for the ptracer to signal that all workers are stopped
                            assert_eq!(read(on_pause_read_fd, &mut [0]).unwrap(), 1);

                            Self::trace();

                            // signal to the ptracer that workers should be resumed
                            write(on_resume_write_fd, "\n".as_bytes()).ok();

                            waitpid(child, None).unwrap();
                        }
                        Ok(ForkResult::Child) => {
                            //write(libc::STDOUT_FILENO, "attaching\n".as_bytes()).ok();

                            for &(pid, _) in workers.iter() {
                                ptrace::attach(pid).unwrap();
                            }

                            // signal to the trace thread that all tasks have been stopped
                            write(on_pause_write_fd, "\n".as_bytes()).ok();

                            // wait for all tracing to be resumed
                            assert_eq!(read(on_resume_read_fd, &mut [0]).unwrap(), 1);

                            for &(pid, _) in workers.iter() {
                                ptrace::detach(pid, None).unwrap();
                            }

                            // don't attempt to unlock workers mutex from child
                            std::mem::forget(workers);

                            unsafe { libc::_exit(0) };
                        }
                        Err(_) => println!("Fork failed"),
                    }
                } else {
                    yield_now();
                }
            }
        });
    }

    pub fn trace() {
        let trace_duration = std::time::Duration::from_secs(3);
        let write_duration = std::time::Duration::from_millis(500);
        let start = std::time::Instant::now();

        while start.elapsed() < trace_duration {
            println!("TRACING!");
            std::thread::sleep(write_duration);
        }
    }
}

static WORKERS: Mutex<Vec<(Pid, Pthread)>> = Mutex::new(Vec::new());

#[derive(Debug)]
struct Worker {
    join_handle: JoinHandle<()>,
    tid: Pid,
    pthread: Pthread,
}

impl Worker {
    pub fn spawn<F>(f: F) -> Self
    where
        F: FnOnce() -> (),
        F: Send + 'static,
    {
        let (sender, receiver) = sync_channel(1);

        let join_handle = std::thread::spawn(move || {
            let tid = gettid();
            let pthread = pthread_self();
            WORKERS.lock().unwrap().push((tid, pthread));
            sender.send((tid, pthread)).unwrap();
            let _on_drop = defer(|| {
                let mut workers = WORKERS.lock().unwrap();
                workers.retain(|ids| ids != &(tid, pthread));
            });
            f()
        });

        let (tid, pthread) = receiver.recv().unwrap();

        Self {
            join_handle,
            tid,
            pthread,
        }
    }
}

mod busy {
    #[inline(never)]
    pub fn work() {
        loop {
            println!("{:?}", std::thread::current().id());

            for i in 0..u8::MAX {
                std::hint::black_box(recurse(i));
            }

            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    #[inline(never)]
    pub fn recurse(depth: u8) {
        if depth == 0 {
            return;
        } else {
            recurse(depth - 1);
        }
    }
}

fn defer<F: FnOnce() -> R, R>(f: F) -> impl Drop {
    struct Defer<F: FnOnce() -> R, R>(Option<F>);

    impl<F: FnOnce() -> R, R> Drop for Defer<F, R> {
        fn drop(&mut self) {
            self.0.take().unwrap()();
        }
    }

    Defer(Some(f))
}
