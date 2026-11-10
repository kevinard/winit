//! Bounded, cancellable native payload I/O. No pipe reads/writes on dispatch.
use rustix::{
    event::{poll, PollFd, PollFlags},
    fs::{fcntl_getfl, fcntl_setfl, OFlags},
};
use std::{
    fs::File,
    io::{self, Read, Write},
    os::fd::OwnedFd,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_TIMEOUT: Duration = Duration::from_secs(5);
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
struct Permit;
impl Drop for Permit {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}
struct State {
    cancelled: AtomicBool,
    finished: AtomicBool,
}

/// Dropping this handle cancels the exact I/O task; never waits on dispatch.
pub struct PipeTransfer(Arc<State>);
impl PipeTransfer {
    pub fn is_finished(&self) -> bool {
        self.0.finished.load(Ordering::Acquire)
    }
}
impl Drop for PipeTransfer {
    fn drop(&mut self) {
        self.0.cancelled.store(true, Ordering::Release);
    }
}

fn check(state: &State, deadline: Instant) -> io::Result<()> {
    if state.cancelled.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Drag data transfer cancelled",
        ))
    } else if Instant::now() >= deadline {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "The drag source or destination did not respond",
        ))
    } else {
        Ok(())
    }
}
fn ready(file: &File, flags: PollFlags, state: &State, deadline: Instant) -> io::Result<()> {
    loop {
        check(state, deadline)?;
        let timeout = (deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(100) as i32)
            .max(1);
        let mut fds = [PollFd::new(file, flags | PollFlags::ERR | PollFlags::HUP)];
        match poll(&mut fds, timeout) {
            Ok(0) => {}
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn start<T: Send + 'static>(
    fd: OwnedFd,
    timeout: Duration,
    operation: impl FnOnce(&mut File, &State, Instant) -> io::Result<T> + Send + 'static,
    done: impl FnOnce(io::Result<T>) + Send + 'static,
) -> io::Result<PipeTransfer> {
    if timeout.is_zero() || timeout > MAX_TIMEOUT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid drag data deadline",
        ));
    }
    ACTIVE
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < 16).then_some(active + 1)
        })
        .map_err(|_| io::Error::other("Drag data workers are busy; try again"))?;
    let permit = Permit;
    let state = Arc::new(State {
        cancelled: AtomicBool::new(false),
        finished: AtomicBool::new(false),
    });
    let worker = state.clone();
    let deadline = Instant::now() + timeout;
    std::thread::Builder::new()
        .name("wayland-drag-data".into())
        .spawn(move || {
            let _permit = permit;
            let mut file = File::from(fd);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                fcntl_setfl(&file, fcntl_getfl(&file)? | OFlags::NONBLOCK)?;
                operation(&mut file, &worker, deadline)
            }))
            .unwrap_or_else(|_| {
                Err(io::Error::other(
                    "The drag data worker stopped unexpectedly",
                ))
            });
            worker.finished.store(true, Ordering::Release);
            done(result);
        })?;
    Ok(PipeTransfer(state))
}

pub fn read_pipe(
    fd: OwnedFd,
    limit: usize,
    timeout: Duration,
    done: impl FnOnce(io::Result<Vec<u8>>) + Send + 'static,
) -> io::Result<PipeTransfer> {
    if limit == 0 || limit > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid drag data size limit",
        ));
    }
    start(
        fd,
        timeout,
        move |file, state, deadline| {
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 64 * 1024];
            loop {
                ready(file, PollFlags::IN, state, deadline)?;
                match file.read(&mut chunk) {
                    Ok(0) => return Ok(bytes),
                    Ok(size) => {
                        if bytes.len().saturating_add(size) > limit {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "The dragged data is too large",
                            ));
                        }
                        bytes.extend_from_slice(&chunk[..size]);
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Err(error),
                }
            }
        },
        done,
    )
}

pub fn write_pipe(
    fd: OwnedFd,
    bytes: Arc<[u8]>,
    timeout: Duration,
    done: impl FnOnce(io::Result<()>) + Send + 'static,
) -> io::Result<PipeTransfer> {
    if bytes.len() > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The dragged data is too large",
        ));
    }
    start(
        fd,
        timeout,
        move |file, state, deadline| {
            let mut offset = 0;
            while offset < bytes.len() {
                ready(file, PollFlags::OUT, state, deadline)?;
                match file.write(&bytes[offset..(offset + 64 * 1024).min(bytes.len())]) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "The drag destination stopped reading",
                        ))
                    }
                    Ok(size) => offset += size,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        },
        done,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Mutex};
    static SERIAL: Mutex<()> = Mutex::new(());
    const WAIT: Duration = Duration::from_secs(2);

    #[test]
    fn pipe_round_trip_preserves_bytes_across_backpressure() {
        let _serial = SERIAL.lock().unwrap();
        let (read, write) = rustix::pipe::pipe().unwrap();
        let bytes: Arc<[u8]> = (0..1024 * 1024)
            .map(|i| (i % 256) as u8)
            .collect::<Vec<_>>()
            .into();
        let (sent, written) = mpsc::channel();
        let writer = write_pipe(write, bytes.clone(), WAIT, move |result| {
            sent.send(result).unwrap();
        })
        .unwrap();
        let (sent, received) = mpsc::channel();
        let reader = read_pipe(read, bytes.len(), WAIT, move |result| {
            sent.send(result).unwrap();
        })
        .unwrap();
        assert_eq!(
            received.recv_timeout(WAIT).unwrap().unwrap().as_slice(),
            bytes.as_ref()
        );
        written.recv_timeout(WAIT).unwrap().unwrap();
        assert!(reader.is_finished() && writer.is_finished());
    }

    #[test]
    fn silent_source_times_out_without_waiting_for_eof() {
        let _serial = SERIAL.lock().unwrap();
        let (read, _write) = rustix::pipe::pipe().unwrap();
        let (sent, received) = mpsc::channel();
        let reader = read_pipe(read, 100, Duration::from_millis(60), move |result| {
            sent.send(result).unwrap();
        })
        .unwrap();
        assert_eq!(
            received.recv_timeout(WAIT).unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(reader.is_finished());
    }

    #[test]
    fn oversized_peer_payload_is_refused() {
        let _serial = SERIAL.lock().unwrap();
        let (read, write) = rustix::pipe::pipe().unwrap();
        let mut write = File::from(write);
        write.write_all(b"12345678").unwrap();
        drop(write);
        let (sent, received) = mpsc::channel();
        let reader = read_pipe(read, 7, WAIT, move |result| {
            sent.send(result).unwrap();
        })
        .unwrap();
        assert_eq!(
            received.recv_timeout(WAIT).unwrap().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(reader.is_finished());
    }

    #[test]
    fn dropping_the_exact_guard_cancels_a_stalled_read() {
        let _serial = SERIAL.lock().unwrap();
        let (read, _write) = rustix::pipe::pipe().unwrap();
        let (sent, received) = mpsc::channel();
        let reader = read_pipe(read, 100, MAX_TIMEOUT, move |result| {
            sent.send(result).unwrap();
        })
        .unwrap();
        drop(reader);
        assert_eq!(
            received.recv_timeout(WAIT).unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn stalled_and_closed_destinations_do_not_block_writers() {
        let _serial = SERIAL.lock().unwrap();
        let (_read, write) = rustix::pipe::pipe().unwrap();
        let (sent, received) = mpsc::channel();
        let writer = write_pipe(
            write,
            vec![42; 1024 * 1024].into(),
            Duration::from_millis(60),
            move |result| {
                sent.send(result).unwrap();
            },
        )
        .unwrap();
        assert_eq!(
            received.recv_timeout(WAIT).unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(writer.is_finished());
        let (read, write) = rustix::pipe::pipe().unwrap();
        drop(read);
        let (sent, received) = mpsc::channel();
        let writer = write_pipe(write, Arc::from(&b"x"[..]), WAIT, move |result| {
            sent.send(result).unwrap();
        })
        .unwrap();
        assert_eq!(
            received.recv_timeout(WAIT).unwrap().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(writer.is_finished());
    }

    #[test]
    fn shared_worker_limit_refuses_and_recovers_after_cancellation() {
        let _serial = SERIAL.lock().unwrap();
        let deadline = Instant::now() + WAIT;
        while ACTIVE.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < deadline, "previous workers did not retire");
            std::thread::yield_now();
        }
        let (sent, received) = mpsc::channel();
        let mut readers = Vec::new();
        let mut peers = Vec::new();
        for _ in 0..16 {
            let (read, write) = rustix::pipe::pipe().unwrap();
            let sent = sent.clone();
            readers.push(
                read_pipe(read, 100, MAX_TIMEOUT, move |result| {
                    sent.send(result).unwrap();
                })
                .unwrap(),
            );
            peers.push(write);
        }
        let (_read, write) = rustix::pipe::pipe().unwrap();
        assert!(write_pipe(write, Arc::from(&b"x"[..]), WAIT, |_| panic!(
            "refused worker ran"
        ))
        .is_err());
        drop(readers);
        for _ in 0..16 {
            assert_eq!(
                received.recv_timeout(WAIT).unwrap().unwrap_err().kind(),
                io::ErrorKind::Interrupted
            );
        }
        let deadline = Instant::now() + WAIT;
        while ACTIVE.load(Ordering::Acquire) != 0 {
            assert!(
                Instant::now() < deadline,
                "cancelled workers did not release capacity"
            );
            std::thread::yield_now();
        }
        let (read, write) = rustix::pipe::pipe().unwrap();
        drop(write);
        let (sent, received) = mpsc::channel();
        let reader = read_pipe(read, 1, WAIT, move |result| {
            sent.send(result).unwrap();
        })
        .unwrap();
        assert!(received.recv_timeout(WAIT).unwrap().unwrap().is_empty());
        assert!(reader.is_finished());
    }
}
