extern crate futures;
extern crate libc;
extern crate std;

use models::*;

use self::std::cmp::min;
use self::std::ffi::CStr;
use self::std::io;
use self::std::io::Write;
use self::std::os::linux::fs::MetadataExt;
use self::std::os::unix::fs::OpenOptionsExt;
use self::std::os::unix::io::AsRawFd;
use self::std::sync::{Arc, Mutex};
use self::std::sync::mpsc::channel;
use self::futures::AsyncSink;
use self::libc::c_char;

const MSG_CHANNEL_SIZE: usize = 1024;

#[repr(C)]
pub struct MSG_CHANNEL {
    buf: [c_char; MSG_CHANNEL_SIZE],
}

impl MSG_CHANNEL {
    pub fn is_empty(&self) -> bool {
        self.buf[0] == 0
    }

    pub fn clear(&mut self) {
        self.buf[0] = 0;
    }

    pub fn peek(&self) -> Option<Vec<u8>> {
        if self.is_empty() {
            None
        } else {
            let mut v: Vec<c_char> = (&self.buf[1..MSG_CHANNEL_SIZE - 2]).into();
            v[MSG_CHANNEL_SIZE - 4] = 0;
            Some(unsafe { CStr::from_ptr(v.as_ptr()) }.to_bytes().into())
        }
    }

    pub fn pop(&mut self) -> Option<Vec<u8>> {
        let v = self.peek();
        self.clear();
        v
    }

    pub fn force_push<T>(&mut self, msg: T)
    where
        T: Into<Vec<u8>>,
    {
        let v = msg.into();
        self.buf[0] = 1;
        for (i, e) in v.iter()
            .enumerate()
            .take(min(v.len(), MSG_CHANNEL_SIZE - 2))
        {
            let c = *e as c_char;
            self.buf[i + 1] = c;
            if c == 0 {
                break;
            }
        }
        self.buf[MSG_CHANNEL_SIZE - 1] = 0;
    }

    pub fn push<T>(&mut self, msg: T) -> AsyncSink<T>
    where
        T: Into<Vec<u8>>,
    {
        if !self.is_empty() {
            AsyncSink::NotReady(msg)
        } else {
            self.force_push(msg);
            AsyncSink::Ready
        }
    }
}

impl Default for MSG_CHANNEL {
    fn default() -> Self {
        Self {
            buf: [0; MSG_CHANNEL_SIZE],
        }
    }
}

#[repr(C)]
#[derive(Default)]
pub struct SHARED_MEM {
    process_control_request: MSG_CHANNEL,
    process_control_reply: MSG_CHANNEL,
    graphics_request: MSG_CHANNEL,
    graphics_reply: MSG_CHANNEL,
    heartbeat: MSG_CHANNEL,
    app_status: MSG_CHANNEL,
    trickle_up: MSG_CHANNEL,
    trickle_down: MSG_CHANNEL,
}

impl SHARED_MEM {
    pub fn get_channel(&self, m: MsgChannel) -> &MSG_CHANNEL {
        match m {
            MsgChannel::ProcessControlRequest => &self.process_control_request,
            MsgChannel::ProcessControlReply => &self.process_control_reply,
            MsgChannel::GraphicsRequest => &self.graphics_request,
            MsgChannel::GraphicsReply => &self.graphics_reply,
            MsgChannel::Heartbeat => &self.heartbeat,
            MsgChannel::AppStatus => &self.app_status,
            MsgChannel::TrickleUp => &self.trickle_up,
            MsgChannel::TrickleDown => &self.trickle_down,
        }
    }
    pub fn get_channel_mut(&mut self, m: MsgChannel) -> &mut MSG_CHANNEL {
        match m {
            MsgChannel::ProcessControlRequest => &mut self.process_control_request,
            MsgChannel::ProcessControlReply => &mut self.process_control_reply,
            MsgChannel::GraphicsRequest => &mut self.graphics_request,
            MsgChannel::GraphicsReply => &mut self.graphics_reply,
            MsgChannel::Heartbeat => &mut self.heartbeat,
            MsgChannel::AppStatus => &mut self.app_status,
            MsgChannel::TrickleUp => &mut self.trickle_up,
            MsgChannel::TrickleDown => &mut self.trickle_down,
        }
    }
}

pub trait AppChannel {
    /// Internal accessor for shared memory.
    fn transaction(&self, f: Box<Fn(&mut SHARED_MEM)>);

    /// Check if `MsgChannel` contains a message.
    fn is_empty(&self, c: MsgChannel) -> bool {
        let (tx, rx) = channel();
        self.transaction(Box::new(move |data| {
            tx.send(data.get_channel(c).is_empty()).unwrap();
        }));
        rx.recv().unwrap()
    }

    /// Check `MsgChannel` contents without extracting.
    fn peek(&self, c: MsgChannel) -> Option<Vec<u8>> {
        let (tx, rx) = channel();
        self.transaction(Box::new(move |data| {
            tx.send(data.get_channel(c).peek()).unwrap();
        }));
        rx.recv().unwrap()
    }

    /// Extract data from the specified `MsgChannel`.
    fn receive(&self, c: MsgChannel) -> Option<Vec<u8>> {
        let (tx, rx) = channel();
        self.transaction(Box::new(move |data| {
            tx.send(data.get_channel_mut(c).pop()).unwrap();
        }));
        rx.recv().unwrap()
    }

    // Receive a new status message from any of the channels, if available
    fn pull_control(&self) -> Option<(ControlMsgChannel, Vec<u8>)> {
        let (tx, rx) = channel();
        self.transaction(Box::new(move |data| {
            for id in ControlMsgChannel::enum_iter() {
                if let Some(v) = data.get_channel_mut(id.into()).pop() {
                    tx.send(Some((id, v))).unwrap();
                    break;
                }
            }
            tx.send(None).unwrap();
        }));
        rx.recv().unwrap()
    }

    // Receive a new status message from any of the channels, if available
    fn pull_status(&self) -> Option<(StatusMsgChannel, Vec<u8>)> {
        let (tx, rx) = channel();
        self.transaction(Box::new(move |data| {
            for id in StatusMsgChannel::enum_iter() {
                if let Some(v) = data.get_channel_mut(id.into()).pop() {
                    tx.send(Some((id, v))).unwrap();
                    break;
                }
            }
            tx.send(None).unwrap();
        }));
        rx.recv().unwrap()
    }

    /// Clear channel contents.
    fn clear(&self, c: MsgChannel) {
        let (tx, rx) = channel();
        self.transaction(Box::new(move |data| {
            tx.send(data.get_channel_mut(c).clear()).unwrap();
        }));
        rx.recv().unwrap()
    }

    /// Send the data to the channel.
    fn push(&self, m: Message) -> AsyncSink<Message> {
        let (c, v) = m.clone().into();
        let (tx, rx) = channel();
        self.transaction(Box::new({
            move |data| {
                tx.send(data.get_channel_mut(c).push(v.clone())).unwrap();
            }
        }));
        rx.recv().unwrap().map(|_| m)
    }

    /// Send the data to the channel. This version does not check message validity and is thus marked unsafe.
    unsafe fn push_unchecked(&self, m: (MsgChannel, Vec<u8>)) -> AsyncSink<(MsgChannel, Vec<u8>)> {
        let (tx, rx) = channel();
        let c = m.0;
        let v = m.1;
        self.transaction(Box::new(move |data| {
            tx.send(data.get_channel_mut(c).push(v.clone())).unwrap();
        }));
        rx.recv().unwrap().map(|v| (c, v))
    }

    /// Overwrite channel contents.
    fn force(&self, m: Message) {
        let (c, v) = m.into();
        let (tx, rx) = channel();
        self.transaction(Box::new(move |data| {
            tx.send(data.get_channel_mut(c).force_push(v.as_slice()))
                .unwrap();
        }));
        rx.recv().unwrap()
    }

    /// Overwrite channel contents. This version does not check message validity and is thus marked unsafe.
    unsafe fn force_unchecked(&self, m: (MsgChannel, Vec<u8>)) {
        let (tx, rx) = channel();
        let c = m.0;
        let v = m.1;
        self.transaction(Box::new(move |data| {
            tx.send(data.get_channel_mut(c).force_push(v.clone()))
                .unwrap();
        }));
        rx.recv().unwrap()
    }
}

#[derive(Default)]
pub struct MemoryAppChannel {
    data: Mutex<SHARED_MEM>,
}

impl AppChannel for MemoryAppChannel {
    fn transaction(&self, f: Box<Fn(&mut SHARED_MEM)>) {
        f(&mut *self.data.lock().unwrap());
    }
}

/// Wrapper to operate on shared mapped memory.
pub struct MmapAppChannel {
    data: Mutex<*mut SHARED_MEM>,
}

impl Drop for MmapAppChannel {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(
                *self.data.lock().unwrap() as *mut libc::c_void,
                std::mem::size_of::<SHARED_MEM>(),
            );
        }
    }
}

impl AppChannel for MmapAppChannel {
    fn transaction(&self, f: Box<Fn(&mut SHARED_MEM)>) {
        let mut p = self.data.lock().unwrap();
        f(unsafe { &mut **p })
    }
}

impl MmapAppChannel {
    pub fn new<P: AsRef<std::path::Path>>(path: P) -> std::io::Result<Self> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .mode(0o666)
            .open(path)?;

        let sz = std::mem::size_of::<SHARED_MEM>();
        let md = f.metadata()?;

        if md.st_size() < sz as u64 {
            f.write_all(&vec![0; sz])?;
        }

        let shmem = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                sz,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_FILE | libc::MAP_SHARED,
                f.as_raw_fd(),
                0,
            )
        };

        if shmem == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            data: Mutex::new(shmem as *mut SHARED_MEM),
        })
    }
}

pub type SharedAppChannel = Arc<AppChannel + Send + Sync + 'static>;