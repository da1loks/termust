use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use eframe::egui;
use russh_sftp::client::{Config, SftpSession};
use russh_sftp::protocol::{FileType, OpenFlags};
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc as async_mpsc, oneshot};
use tokio::time::timeout;
use uuid::Uuid;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::connection::Launch;
use crate::credentials::Askpass;

const MAX_PACKET: u32 = 256 * 1024;
const REMOTE_TIMEOUT: u64 = 20;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Directory,
    File,
    Link,
    Other,
}

#[derive(Clone)]
pub(super) struct Entry<P> {
    pub name: String,
    pub path: P,
    pub kind: Kind,
    pub size: Option<u64>,
}

pub(super) struct Listing<P> {
    pub path: P,
    pub parent: Option<P>,
    pub entries: Vec<Entry<P>>,
}

pub(super) enum Request {
    Remote(String),
    Local(PathBuf),
    Download {
        source: String,
        directory: PathBuf,
        name: String,
    },
    Upload {
        sources: Vec<PathBuf>,
        directory: String,
    },
}

type Reply<T> = Result<T, String>;

#[derive(Default)]
pub(super) struct TransferReport {
    pub completed: usize,
    pub first_error: Option<String>,
}

impl TransferReport {
    fn record(&mut self, result: Result<()>) {
        match result {
            Ok(()) => self.completed += 1,
            Err(error) => {
                self.first_error.get_or_insert_with(|| format!("{error:#}"));
            }
        }
    }
}

pub(super) enum Update {
    Connected(Reply<Listing<String>>),
    Remote(Reply<Listing<String>>),
    Local(Reply<Listing<PathBuf>>),
    Transferred(TransferReport),
    Disconnected(String),
}

pub(super) struct Worker {
    pub requests: async_mpsc::Sender<Request>,
    pub updates: mpsc::Receiver<Update>,
    pub bytes: Arc<AtomicU64>,
    _close: oneshot::Sender<()>,
}

struct Reporter {
    updates: mpsc::Sender<Update>,
    context: egui::Context,
    active: Arc<AtomicU64>,
    id: u64,
    bytes: Arc<AtomicU64>,
}

impl Reporter {
    fn update(&self, message: Update) {
        if self.updates.send(message).is_ok() {
            self.wake();
        }
    }

    fn wake(&self) {
        if self.active.load(Ordering::Relaxed) == self.id {
            self.context.request_repaint();
        }
    }
}

impl Worker {
    pub fn spawn(
        launch: Launch,
        id: u64,
        active: Arc<AtomicU64>,
        context: egui::Context,
    ) -> Result<Self> {
        let (requests, inbox) = async_mpsc::channel(1);
        let (outbox, updates) = mpsc::channel();
        let (_close, close) = oneshot::channel();
        let bytes = Arc::new(AtomicU64::new(0));
        let reporter = Reporter {
            updates: outbox,
            context,
            active,
            id,
            bytes: bytes.clone(),
        };
        std::thread::Builder::new()
            .name(format!("sftp-{id}"))
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("Не удалось запустить SFTP")
                    .and_then(|runtime| runtime.block_on(run(launch, inbox, close, &reporter)));
                if let Err(error) = result {
                    reporter.update(Update::Disconnected(format!("{error:#}")));
                }
            })
            .context("Не удалось создать сеанс SFTP")?;
        Ok(Self {
            requests,
            updates,
            bytes,
            _close,
        })
    }
}

async fn run(
    launch: Launch,
    mut requests: async_mpsc::Receiver<Request>,
    mut close: oneshot::Receiver<()>,
    reporter: &Reporter,
) -> Result<()> {
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let initial = home.as_deref().unwrap_or_else(|| Path::new("."));
    let listing = tokio::select! {
        _ = &mut close => return Ok(()),
        result = list_local(initial) => result,
    };
    reporter.update(Update::Local(reply(listing)));

    let mut child = start_ssh(&launch, home.as_deref())?;
    let stdout = child.stdout.take().context("Нет выхода OpenSSH")?;
    let stdin = child.stdin.take().context("Нет входа OpenSSH")?;
    let mut stderr = child
        .stderr
        .take()
        .context("Нет диагностического канала OpenSSH")?;
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let capture = diagnostics.clone();
    tokio::spawn(async move {
        let mut buffer = [0; 1024];
        while let Ok(n) = stderr.read(&mut buffer).await {
            if n == 0 {
                break;
            }
            let mut log = capture.lock().unwrap();
            log.extend_from_slice(&buffer[..n]);
            let excess = log.len().saturating_sub(4096);
            log.drain(..excess);
        }
    });
    let (incoming, forwarded) = io::duplex(64 * 1024);
    tokio::spawn(forward_packets(stdout, forwarded));
    let config = Config {
        max_packet_len: MAX_PACKET,
        request_timeout_secs: 120,
        ..Config::default()
    };
    let session = tokio::select! {
        _ = &mut close => return Ok(()),
        result = SftpSession::new_with_config(io::join(incoming, stdin), config) => result,
    };
    let session = match session {
        Ok(session) => session,
        Err(err) => {
            let _ = timeout(Duration::from_millis(150), child.wait()).await;
            let message = ssh_diagnostic(&diagnostics);
            return Err(err).context(if message.is_empty() {
                "Не удалось открыть подсистему SFTP".into()
            } else {
                format!("Не удалось открыть SFTP\n{message}")
            });
        }
    };
    session.set_timeout(REMOTE_TIMEOUT);
    let initial = tokio::select! {
        _ = &mut close => return Ok(()),
        listing = list_remote(&session, ".") => listing,
    };
    reporter.update(Update::Connected(reply(initial)));

    loop {
        let request = tokio::select! {
            _ = &mut close => break,
            request = requests.recv() => match request {
                Some(request) => request,
                None => break,
            },
            status = child.wait() => {
                let status = status.context("Не удалось проверить процесс OpenSSH")?;
                let details = ssh_diagnostic(&diagnostics);
                if details.is_empty() {
                    bail!("Сеанс SFTP завершён ({status})");
                }
                bail!("Сеанс SFTP завершён\n{details}");
            }
        };
        let mut staging = None;
        let result = tokio::select! {
            biased;
            _ = &mut close => None,
            result = perform(&session, request, &mut staging, reporter) => Some(result),
        };
        clean_partial(&session, &mut staging).await;
        match result {
            Some(update) => reporter.update(update),
            None => break,
        }
    }
    let _ = session.close().await;
    Ok(())
}

fn start_ssh(launch: &Launch, home: Option<&Path>) -> Result<Child> {
    let askpass = launch.askpass.clone().unwrap_or_else(Askpass::manual);
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .envs(askpass.environment()?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .kill_on_drop(true);
    if let Some(home) = home.filter(|path| path.is_dir()) {
        command.current_dir(home);
    }
    command
        .spawn()
        .context("Не удалось запустить OpenSSH для SFTP")
}

fn ssh_diagnostic(log: &Mutex<Vec<u8>>) -> String {
    String::from_utf8_lossy(&log.lock().unwrap())
        .trim()
        .to_owned()
}

// Check the frame length before passing it to the SFTP decoder.
async fn forward_packets(
    mut reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
) -> io::Result<()> {
    let mut payload = Vec::new();
    loop {
        let length = match reader.read_u32().await {
            Ok(length) => length,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(err) => return Err(err),
        };
        if !(1..=MAX_PACKET).contains(&length) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid SFTP packet length",
            ));
        }
        payload.resize(length as usize, 0);
        reader.read_exact(&mut payload).await?;
        writer.write_u32(length).await?;
        writer.write_all(&payload).await?;
    }
    writer.shutdown().await
}

async fn perform(
    session: &SftpSession,
    request: Request,
    staging: &mut Option<String>,
    reporter: &Reporter,
) -> Update {
    match request {
        Request::Local(path) => Update::Local(reply(list_local(&path).await)),
        Request::Remote(path) => Update::Remote(reply(list_remote(session, &path).await)),
        Request::Download {
            source,
            directory,
            name,
        } => {
            let mut report = TransferReport::default();
            report.record(download(session, &source, &directory, &name, reporter).await);
            Update::Transferred(report)
        }
        Request::Upload { sources, directory } => {
            let mut report = TransferReport::default();
            for source in sources {
                let result = upload(session, &source, &directory, staging, reporter)
                    .await
                    .with_context(|| format!("Ошибка загрузки {}", source.display()));
                report.record(result);
                clean_partial(session, staging).await;
                reporter.wake();
            }
            Update::Transferred(report)
        }
    }
}

async fn clean_partial(session: &SftpSession, staging: &mut Option<String>) {
    if let Some(path) = staging.as_deref() {
        let _ = timeout(Duration::from_secs(3), session.remove_file(path)).await;
        *staging = None;
    }
}

fn reply<T>(result: Result<T>) -> Reply<T> {
    result.map_err(|error| format!("{error:#}"))
}

async fn list_local(path: &Path) -> Result<Listing<PathBuf>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_local(&path))
        .await
        .context("Не удалось получить список локальных файлов")?
}

fn read_local(path: &Path) -> Result<Listing<PathBuf>> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("Не удалось открыть {}", path.display()))?;
    let reader = std::fs::read_dir(&path)
        .with_context(|| format!("Не удалось прочитать {}", path.display()))?;
    let mut entries = Vec::new();
    for entry in reader {
        let entry = entry?;
        let kind = match entry.file_type() {
            Ok(kind) if kind.is_dir() => Kind::Directory,
            Ok(kind) if kind.is_file() => Kind::File,
            Ok(kind) if kind.is_symlink() => Kind::Link,
            _ => Kind::Other,
        };
        let size = entry
            .metadata()
            .ok()
            .filter(|meta| meta.is_file())
            .map(|meta| meta.len());
        entries.push(Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: entry.path(),
            kind,
            size,
        });
    }
    sort_entries(&mut entries);
    Ok(Listing {
        parent: path.parent().map(Path::to_path_buf),
        path,
        entries,
    })
}

async fn list_remote(session: &SftpSession, path: &str) -> Result<Listing<String>> {
    let path = session
        .canonicalize(path)
        .await
        .with_context(|| format!("Не удалось открыть {path}"))?;
    let mut entries = Vec::new();
    for entry in session
        .read_dir(path.as_str())
        .await
        .with_context(|| format!("Не удалось прочитать {path}"))?
    {
        let name = entry.file_name();
        if name.is_empty() || name.contains(['/', '\0']) {
            continue;
        }
        let kind = match entry.file_type() {
            FileType::Dir => Kind::Directory,
            FileType::File => Kind::File,
            FileType::Symlink => Kind::Link,
            FileType::Other => Kind::Other,
        };
        entries.push(Entry {
            path: remote_child(&path, &name),
            name,
            kind,
            size: entry.metadata().size,
        });
    }
    sort_entries(&mut entries);
    let parent = path
        .trim_end_matches('/')
        .rsplit_once('/')
        .map(|(parent, _)| {
            if parent.is_empty() {
                "/".into()
            } else {
                parent.into()
            }
        });
    Ok(Listing {
        path,
        parent,
        entries,
    })
}

fn sort_entries<P>(entries: &mut [Entry<P>]) {
    entries.sort_by_cached_key(|entry| (entry.kind != Kind::Directory, entry.name.to_lowercase()));
}

pub(super) fn remote_child(directory: &str, name: &str) -> String {
    format!("{}/{name}", directory.trim_end_matches('/'))
}

async fn download(
    session: &SftpSession,
    source: &str,
    directory: &Path,
    name: &str,
    reporter: &Reporter,
) -> Result<()> {
    ensure!(
        !name.is_empty()
            && !name.ends_with(['.', ' '])
            && !name
                .chars()
                .any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c))
            && !windows_device_name(name),
        "Имя файла несовместимо с Windows"
    );
    let target = directory.join(name);
    ensure!(
        target.symlink_metadata().is_err(),
        "Файл с таким именем уже есть в локальной папке"
    );
    let mut remote = session
        .open(source)
        .await
        .context("Не удалось открыть файл на сервере")?;
    let output =
        tempfile::NamedTempFile::new_in(directory).context("Не удалось создать локальный файл")?;
    let mut file = tokio::fs::File::from_std(output.reopen()?);
    copy_file(&mut remote, &mut file, reporter).await?;
    remote.close().await?;
    file.sync_all().await?;
    drop(file);
    output
        .persist_noclobber(target)
        .context("Не удалось сохранить скачанный файл")?;
    Ok(())
}

fn windows_device_name(name: &str) -> bool {
    let first = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    matches!(
        first.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (first.starts_with("COM") || first.starts_with("LPT"))
        && matches!(
            &first[3..],
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        )
}

async fn upload(
    session: &SftpSession,
    source: &Path,
    directory: &str,
    staging: &mut Option<String>,
    reporter: &Reporter,
) -> Result<()> {
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .context("Имя файла несовместимо с SFTP")?;
    let metadata = tokio::fs::symlink_metadata(source)
        .await
        .context("Не удалось прочитать свойства локального файла")?;
    ensure!(metadata.is_file(), "Для загрузки выбери обычный файл");
    let destination = remote_child(directory, name);
    ensure!(
        !session.try_exists(&destination).await?,
        "Файл с таким именем уже есть на сервере"
    );
    let mut local = tokio::fs::File::open(source)
        .await
        .context("Не удалось открыть локальный файл")?;
    let partial = remote_child(directory, &format!(".termust-{}.part", Uuid::new_v4()));
    let mut remote = session
        .open_with_flags(
            &partial,
            OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::EXCLUDE,
        )
        .await
        .context("Не удалось создать файл на сервере")?;
    *staging = Some(partial.clone());
    copy_file(&mut local, &mut remote, reporter).await?;
    remote.close().await?;
    session
        .rename(partial, destination)
        .await
        .context("Не удалось сохранить файл на сервере")?;
    *staging = None;
    Ok(())
}

async fn copy_file(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    reporter: &Reporter,
) -> Result<()> {
    let mut buffer = vec![0; 128 * 1024];
    let mut last = Instant::now();
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .context("Не удалось прочитать файл")?;
        if read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..read])
            .await
            .context("Не удалось записать файл")?;
        reporter.bytes.fetch_add(read as u64, Ordering::Relaxed);
        if last.elapsed() >= Duration::from_millis(100) {
            reporter.wake();
            last = Instant::now();
        }
    }
    writer
        .flush()
        .await
        .context("Не удалось завершить запись")?;
    Ok(())
}
