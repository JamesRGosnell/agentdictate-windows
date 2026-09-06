use agentdictate_core::{ClientCommand, PROTOCOL_VERSION, ServerMessage};
use std::{
    fs::{self, File, OpenOptions},
    hash::{Hash, Hasher},
    io::{self, BufRead, BufReader, Read, Write},
    os::windows::io::{AsRawHandle, FromRawHandle},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread::JoinHandle,
};
use thiserror::Error;
use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*, System::Pipes::*};
const MAX_FRAME_BYTES: usize = 1024 * 1024;
#[derive(Debug, Error)]
pub enum IpcError {
    #[error("IPC I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("IPC message is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("IPC peer uses protocol {received}, expected {expected}")]
    ProtocolVersion { received: u16, expected: u16 },
    #[error("IPC peer disconnected")]
    Disconnected,
    #[error("AgentDictate is already listening at {path}")]
    AlreadyRunning { path: PathBuf },
}
pub trait IpcHandler {
    fn snapshot(&self, request_id: u64) -> ServerMessage;
    fn handle(&mut self, command: ClientCommand) -> ServerMessage;
}
fn pipe_name(path: &Path) -> Vec<u16> {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    path.as_os_str()
        .to_string_lossy()
        .to_lowercase()
        .hash(&mut hash);
    format!(r"\\.\pipe\agentdictate-{:016x}", hash.finish())
        .encode_utf16()
        .chain(Some(0))
        .collect()
}
fn create_pipe(name: &[u16], first: bool) -> io::Result<File> {
    let security = super::windows_security::SecurityDescriptor::current_user()?;
    let attributes = security.attributes();
    // SAFETY: name is terminated, attributes and descriptor outlive CreateNamedPipeW.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX
                | if first {
                    FILE_FLAG_FIRST_PIPE_INSTANCE
                } else {
                    0
                },
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            65536,
            65536,
            1000,
            &attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ownership of the new pipe handle transfers exactly once to File.
    Ok(unsafe { File::from_raw_handle(handle) })
}
pub struct IpcServer {
    name: Vec<u16>,
    pending: Mutex<File>,
    _singleton_lock: File,
}
impl IpcServer {
    pub fn bind(runtime_directory: impl AsRef<Path>) -> Result<Self, IpcError> {
        let path = runtime_directory.as_ref();
        fs::create_dir_all(path)?;
        super::windows_security::restrict_path(path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.join("agentdictate.lock"))?;
        lock.try_lock().map_err(|_| IpcError::AlreadyRunning {
            path: path.to_owned(),
        })?;
        let name = pipe_name(path);
        let pending = Mutex::new(create_pipe(&name, true)?);
        Ok(Self {
            name,
            pending,
            _singleton_lock: lock,
        })
    }
    fn accept(&self) -> io::Result<File> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| io::Error::other("IPC accept lock poisoned"))?;
        // SAFETY: pending owns a live synchronous named pipe server handle.
        if unsafe { ConnectNamedPipe(pending.as_raw_handle(), std::ptr::null_mut()) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                return Err(error);
            }
        }
        let next = create_pipe(&self.name, false)?;
        Ok(std::mem::replace(&mut *pending, next))
    }
    pub fn serve_next(&self, handler: &mut impl IpcHandler) -> Result<(), IpcError> {
        let mut stream = self.accept()?;
        write_message(&mut stream, &handler.snapshot(0))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        while let Some(command) = read_message::<ClientCommand>(&mut reader)? {
            check_version(command.protocol_version)?;
            let response = handler.handle(command);
            check_version(response.protocol_version)?;
            write_message(&mut stream, &response)?;
        }
        Ok(())
    }
    pub fn serve_next_concurrent<H: IpcHandler + Send + 'static>(
        &self,
        handler: Arc<Mutex<H>>,
    ) -> Result<JoinHandle<Result<(), IpcError>>, IpcError> {
        let stream = self.accept()?;
        Ok(std::thread::spawn(move || serve_shared(stream, &handler)))
    }
}
pub struct IpcClient {
    stream: File,
    reader: BufReader<File>,
}
fn connect_pipe(path: &Path) -> io::Result<File> {
    let name = pipe_name(path);
    for _ in 0..20 {
        // SAFETY: terminated name, no inherited handle, synchronous read/write access.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(unsafe { File::from_raw_handle(handle) });
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
            return Err(error);
        }
        unsafe {
            WaitNamedPipeW(name.as_ptr(), 250);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "daemon pipe is busy",
    ))
}
impl IpcClient {
    pub fn connect(path: impl AsRef<Path>) -> Result<(Self, ServerMessage), IpcError> {
        let stream = connect_pipe(path.as_ref())?;
        let reader = BufReader::new(stream.try_clone()?);
        let mut client = Self { stream, reader };
        let initial = client.read_server_message()?;
        Ok((client, initial))
    }
    pub fn send(&mut self, command: ClientCommand) -> Result<ServerMessage, IpcError> {
        check_version(command.protocol_version)?;
        write_message(&mut self.stream, &command)?;
        self.read_server_message()
    }
    pub fn peer_pid(&self) -> Result<u32, IpcError> {
        let mut pid = 0;
        if unsafe { GetNamedPipeServerProcessId(self.stream.as_raw_handle(), &mut pid) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(pid)
    }
    pub fn wake(path: impl AsRef<Path>) -> Result<(), IpcError> {
        drop(connect_pipe(path.as_ref())?);
        Ok(())
    }
    fn read_server_message(&mut self) -> Result<ServerMessage, IpcError> {
        let message: ServerMessage =
            read_message(&mut self.reader)?.ok_or(IpcError::Disconnected)?;
        check_version(message.protocol_version)?;
        Ok(message)
    }
}
fn write_message(writer: &mut impl Write, message: &impl serde::Serialize) -> Result<(), IpcError> {
    serde_json::to_writer(&mut *writer, message)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn read_message<T: serde::de::DeserializeOwned>(
    reader: &mut impl BufRead,
) -> Result<Option<T>, IpcError> {
    let mut line = String::new();
    let read = match reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_line(&mut line)
    {
        Ok(read) => read,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if read == 0 {
        return Ok(None);
    }
    if line.len() > MAX_FRAME_BYTES || !line.ends_with('\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "IPC frame exceeds the 1 MiB limit or is incomplete",
        )
        .into());
    }
    Ok(Some(serde_json::from_str(&line)?))
}

fn serve_shared<H>(mut stream: File, handler: &Arc<Mutex<H>>) -> Result<(), IpcError>
where
    H: IpcHandler,
{
    let initial = handler
        .lock()
        .map_err(|_| std::io::Error::other("IPC handler lock is poisoned"))?
        .snapshot(0);
    write_message(&mut stream, &initial)?;
    let reader_stream = stream.try_clone()?;
    let mut reader = BufReader::new(reader_stream);
    while let Some(command) = read_message::<ClientCommand>(&mut reader)? {
        check_version(command.protocol_version)?;
        let response = handler
            .lock()
            .map_err(|_| std::io::Error::other("IPC handler lock is poisoned"))?
            .handle(command);
        check_version(response.protocol_version)?;
        write_message(&mut stream, &response)?;
    }
    Ok(())
}

fn check_version(received: u16) -> Result<(), IpcError> {
    if received == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(IpcError::ProtocolVersion {
            received,
            expected: PROTOCOL_VERSION,
        })
    }
}
