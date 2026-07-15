//! Bounded, non-blocking file logging.
//!
//! Trace, debug, and info records share a soft queue limit. When that limit is
//! reached, producers drop those records and atomically coalesce counts by
//! level. The writer reports the counts directly to the log file, without
//! recursively invoking the logger. Warning and error records bypass the soft
//! limit and use reserved queue capacity. If even that reserve is exhausted,
//! their counts are reported separately at error level. Record producers only
//! use `try_send`; explicit flush and shutdown controls wait while the writer
//! drains accepted records.

use std::{
    fmt::Write as FmtWrite,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write as IoWrite},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

const MAX_LOG_BYTES: u64 = 32 * 1024 * 1024;
const LOG_BACKUPS: usize = 3;
const LOG_BUFFER_BYTES: usize = 16 * 1024;
const LOG_QUEUE_CAPACITY: usize = 2_048;
const LOG_EMERGENCY_RESERVE: usize = 64;
const LOG_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const MAX_LOG_TARGET_BYTES: usize = 512;
const MAX_LOG_MESSAGE_BYTES: usize = 16 * 1024;
const TRUNCATION_MARKER: &str = "...<truncated>";

pub struct FlushGuard;

impl Drop for FlushGuard {
    fn drop(&mut self) {
        flush();
    }
}

pub fn setup(verbosity: u64, path: &Path) -> anyhow::Result<FlushGuard> {
    let level = match verbosity {
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Info,
        2 => log::LevelFilter::Debug,
        _3_or_more => log::LevelFilter::Trace,
    };

    let logger = AsyncFileLogger::open(path, level)?;
    log::set_boxed_logger(Box::new(logger))
        .map_err(|_| anyhow::anyhow!("global logger is already initialized"))?;
    log::set_max_level(level);
    Ok(FlushGuard)
}

/// Drain all accepted records and flush the log file.
///
/// Lifecycle code must call this before `std::process::exit`, which does not
/// run the global logger's destructor.
pub fn flush() {
    log::logger().flush();
}

struct AsyncFileLogger {
    level: log::LevelFilter,
    queue: LogQueue,
    worker: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
struct AsyncFileLoggerConfig {
    max_bytes: u64,
    backups: usize,
    queue_capacity: usize,
    emergency_reserve: usize,
    flush_interval: Duration,
}

impl Default for AsyncFileLoggerConfig {
    fn default() -> Self {
        Self {
            max_bytes: MAX_LOG_BYTES,
            backups: LOG_BACKUPS,
            queue_capacity: LOG_QUEUE_CAPACITY,
            emergency_reserve: LOG_EMERGENCY_RESERVE,
            flush_interval: LOG_FLUSH_INTERVAL,
        }
    }
}

impl AsyncFileLogger {
    fn open(path: &Path, level: log::LevelFilter) -> io::Result<Self> {
        Self::open_with_config(path, level, AsyncFileLoggerConfig::default())
    }

    fn open_with_config(
        path: &Path,
        level: log::LevelFilter,
        config: AsyncFileLoggerConfig,
    ) -> io::Result<Self> {
        let (queue, receiver) = LogQueue::bounded(config.queue_capacity, config.emergency_reserve)?;
        let queued_records = queue.queued_records.clone();
        let dropped = queue.dropped.clone();
        let path = path.to_path_buf();
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let worker = thread::Builder::new()
            .name("helix-log-writer".to_owned())
            .spawn(move || {
                let output = match RotatingFile::open(&path, config.max_bytes, config.backups) {
                    Ok(output) => {
                        if ready_tx.send(Ok(())).is_err() {
                            return;
                        }
                        output
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                writer_loop(
                    output,
                    receiver,
                    queued_records,
                    dropped,
                    config.flush_interval,
                );
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = worker.join();
                return Err(error);
            }
            Err(_) => {
                let _ = worker.join();
                return Err(writer_unavailable());
            }
        }

        Ok(Self {
            level,
            queue,
            worker: Some(worker),
        })
    }

    fn flush_result(&self) -> io::Result<()> {
        let (complete_tx, complete_rx) = mpsc::channel();
        self.queue.send_control(WriterCommand::Flush(complete_tx))?;
        complete_rx
            .recv()
            .map_err(|_| writer_unavailable())?
            .map_err(io::Error::other)
    }

    fn shutdown(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };

        let (complete_tx, complete_rx) = mpsc::channel();
        if self
            .queue
            .send_control(WriterCommand::Shutdown(complete_tx))
            .is_ok()
        {
            let _ = complete_rx.recv();
        }
        let _ = worker.join();
    }
}

impl log::Log for AsyncFileLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            self.queue
                .try_enqueue_with(record.level(), || QueuedRecord::from_log(record));
        }
    }

    fn flush(&self) {
        let _ = self.flush_result();
    }
}

impl Drop for AsyncFileLogger {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn writer_unavailable() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "log writer thread is unavailable",
    )
}

#[derive(Debug)]
struct QueuedRecord {
    timestamp: SystemTime,
    level: log::Level,
    target: Box<str>,
    message: Box<str>,
}

impl QueuedRecord {
    fn from_log(record: &log::Record<'_>) -> Self {
        Self {
            timestamp: SystemTime::now(),
            level: record.level(),
            target: bounded_str(record.target(), MAX_LOG_TARGET_BYTES),
            message: bounded_string(record.args().to_string(), MAX_LOG_MESSAGE_BYTES),
        }
    }

    fn internal(level: log::Level, message: String) -> Self {
        Self {
            timestamp: SystemTime::now(),
            level,
            target: "helix_term::logging".into(),
            message: message.into_boxed_str(),
        }
    }
}

fn bounded_str(value: &str, max_bytes: usize) -> Box<str> {
    if value.len() <= max_bytes {
        return value.into();
    }

    let keep_bytes = truncation_boundary(value, max_bytes.saturating_sub(TRUNCATION_MARKER.len()));
    let mut bounded = String::with_capacity(keep_bytes + TRUNCATION_MARKER.len());
    bounded.push_str(&value[..keep_bytes]);
    bounded.push_str(TRUNCATION_MARKER);
    bounded.into_boxed_str()
}

fn bounded_string(mut value: String, max_bytes: usize) -> Box<str> {
    if value.len() > max_bytes {
        let keep_bytes =
            truncation_boundary(&value, max_bytes.saturating_sub(TRUNCATION_MARKER.len()));
        value.truncate(keep_bytes);
        value.push_str(TRUNCATION_MARKER);
    }
    value.into_boxed_str()
}

fn truncation_boundary(value: &str, mut boundary: usize) -> usize {
    boundary = boundary.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

enum WriterCommand {
    Record(QueuedRecord),
    Flush(mpsc::Sender<Result<(), String>>),
    Shutdown(mpsc::Sender<Result<(), String>>),
}

#[derive(Clone)]
struct LogQueue {
    sender: SyncSender<WriterCommand>,
    queued_records: Arc<AtomicUsize>,
    dropped: Arc<DroppedRecords>,
    low_priority_limit: usize,
    capacity: usize,
}

impl LogQueue {
    fn bounded(
        capacity: usize,
        emergency_reserve: usize,
    ) -> io::Result<(Self, Receiver<WriterCommand>)> {
        if capacity == 0 || emergency_reserve >= capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log queue capacity must exceed its emergency reserve",
            ));
        }

        let (sender, receiver) = mpsc::sync_channel(capacity);
        Ok((
            Self {
                sender,
                queued_records: Arc::new(AtomicUsize::new(0)),
                dropped: Arc::new(DroppedRecords::default()),
                low_priority_limit: capacity - emergency_reserve,
                capacity,
            },
            receiver,
        ))
    }

    #[cfg(test)]
    fn try_enqueue(&self, record: QueuedRecord) -> bool {
        self.try_enqueue_with(record.level, || record)
    }

    fn try_enqueue_with(
        &self,
        level: log::Level,
        build_record: impl FnOnce() -> QueuedRecord,
    ) -> bool {
        let limit = if is_low_priority(level) {
            self.low_priority_limit
        } else {
            self.capacity
        };

        if self
            .queued_records
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                (queued < limit).then_some(queued + 1)
            })
            .is_err()
        {
            self.dropped.increment(level);
            return false;
        }
        let reservation = QueueReservation {
            queued_records: &self.queued_records,
            release_on_drop: true,
        };
        let record = build_record();

        match self.sender.try_send(WriterCommand::Record(record)) {
            Ok(()) => {
                reservation.commit();
                true
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.dropped.increment(level);
                false
            }
        }
    }

    fn send_control(&self, command: WriterCommand) -> io::Result<()> {
        self.sender.send(command).map_err(|_| writer_unavailable())
    }
}

struct QueueReservation<'a> {
    queued_records: &'a AtomicUsize,
    release_on_drop: bool,
}

impl QueueReservation<'_> {
    fn commit(mut self) {
        self.release_on_drop = false;
    }
}

impl Drop for QueueReservation<'_> {
    fn drop(&mut self) {
        if self.release_on_drop {
            self.queued_records.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

fn is_low_priority(level: log::Level) -> bool {
    matches!(
        level,
        log::Level::Trace | log::Level::Debug | log::Level::Info
    )
}

#[derive(Default)]
struct DroppedRecords {
    trace: AtomicU64,
    debug: AtomicU64,
    info: AtomicU64,
    warning: AtomicU64,
    error: AtomicU64,
}

impl DroppedRecords {
    fn increment(&self, level: log::Level) {
        let counter = match level {
            log::Level::Trace => &self.trace,
            log::Level::Debug => &self.debug,
            log::Level::Info => &self.info,
            log::Level::Warn => &self.warning,
            log::Level::Error => &self.error,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn take(&self) -> DroppedSnapshot {
        DroppedSnapshot {
            trace: self.trace.swap(0, Ordering::AcqRel),
            debug: self.debug.swap(0, Ordering::AcqRel),
            info: self.info.swap(0, Ordering::AcqRel),
            warning: self.warning.swap(0, Ordering::AcqRel),
            error: self.error.swap(0, Ordering::AcqRel),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DroppedSnapshot {
    trace: u64,
    debug: u64,
    info: u64,
    warning: u64,
    error: u64,
}

impl DroppedSnapshot {
    fn has_low_priority(&self) -> bool {
        self.trace != 0 || self.debug != 0 || self.info != 0
    }

    fn has_high_priority(&self) -> bool {
        self.warning != 0 || self.error != 0
    }
}

fn writer_loop(
    mut output: RotatingFile,
    receiver: Receiver<WriterCommand>,
    queued_records: Arc<AtomicUsize>,
    dropped: Arc<DroppedRecords>,
    flush_interval: Duration,
) {
    let mut last_flush = Instant::now();
    let mut pending_error = None;

    loop {
        match receiver.recv_timeout(flush_interval) {
            Ok(WriterCommand::Record(record)) => {
                queued_records.fetch_sub(1, Ordering::AcqRel);
                remember_writer_error(
                    &mut pending_error,
                    write_queued_record(&mut output, &record),
                );
                if last_flush.elapsed() >= flush_interval {
                    remember_writer_error(
                        &mut pending_error,
                        report_dropped(&mut output, &dropped),
                    );
                    remember_writer_error(&mut pending_error, output.flush());
                    last_flush = Instant::now();
                }
            }
            Ok(WriterCommand::Flush(complete)) => {
                let result = flush_output(&mut output, &dropped, &mut pending_error);
                last_flush = Instant::now();
                let _ = complete.send(result);
            }
            Ok(WriterCommand::Shutdown(complete)) => {
                let result = flush_output(&mut output, &dropped, &mut pending_error);
                let _ = complete.send(result);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                remember_writer_error(&mut pending_error, report_dropped(&mut output, &dropped));
                remember_writer_error(&mut pending_error, output.flush());
                last_flush = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = flush_output(&mut output, &dropped, &mut pending_error);
                break;
            }
        }
    }
}

fn remember_writer_error(pending_error: &mut Option<String>, result: io::Result<()>) {
    if let Err(error) = result {
        if pending_error.is_none() {
            *pending_error = Some(error.to_string());
        }
        let _ = writeln!(io::stderr().lock(), "helix log writer error: {error}");
    }
}

fn flush_output(
    output: &mut RotatingFile,
    dropped: &DroppedRecords,
    pending_error: &mut Option<String>,
) -> Result<(), String> {
    remember_writer_error(pending_error, report_dropped(output, dropped));
    remember_writer_error(pending_error, output.flush());
    pending_error.take().map_or(Ok(()), Err)
}

fn report_dropped(output: &mut RotatingFile, dropped: &DroppedRecords) -> io::Result<()> {
    let dropped = dropped.take();
    if dropped.has_low_priority() {
        let message = format!(
            "log queue saturated; dropped trace={} debug={} info={}",
            dropped.trace, dropped.debug, dropped.info
        );
        write_queued_record(output, &QueuedRecord::internal(log::Level::Warn, message))?;
    }
    if dropped.has_high_priority() {
        let message = format!(
            "log emergency reserve saturated; dropped warning={} error={}",
            dropped.warning, dropped.error
        );
        write_queued_record(output, &QueuedRecord::internal(log::Level::Error, message))?;
    }
    Ok(())
}

fn write_queued_record(output: &mut RotatingFile, record: &QueuedRecord) -> io::Result<()> {
    let timestamp: chrono::DateTime<chrono::Local> = record.timestamp.into();
    let mut line = String::with_capacity(record.target.len() + record.message.len() + 64);
    writeln!(
        line,
        "{} {} [{}] {}",
        timestamp.format("%Y-%m-%dT%H:%M:%S%.3f"),
        record.target,
        record.level,
        record.message
    )
    .expect("writing to a String cannot fail");
    output.write_record(line.as_bytes())
}

struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    backups: usize,
    bytes_written: u64,
    writer: Option<BufWriter<File>>,
}

impl RotatingFile {
    fn open(path: &Path, max_bytes: u64, backups: usize) -> io::Result<Self> {
        if max_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "maximum log size must be greater than zero",
            ));
        }

        if path
            .metadata()
            .is_ok_and(|metadata| metadata.len() >= max_bytes)
        {
            rotate_files(path, backups)?;
        }

        let file = open_append(path)?;
        let bytes_written = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            max_bytes,
            backups,
            bytes_written,
            writer: Some(BufWriter::with_capacity(LOG_BUFFER_BYTES, file)),
        })
    }

    fn write_record(&mut self, buffer: &[u8]) -> io::Result<()> {
        if self.bytes_written > 0
            && self.bytes_written.saturating_add(buffer.len() as u64) > self.max_bytes
        {
            self.rotate()?;
        }

        self.writer
            .as_mut()
            .expect("rotating log writer must remain open")
            .write_all(buffer)?;
        self.bytes_written = self.bytes_written.saturating_add(buffer.len() as u64);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        let mut writer = self
            .writer
            .take()
            .expect("rotating log writer must remain open");
        if let Err(error) = writer.flush() {
            self.writer = Some(writer);
            return Err(error);
        }
        drop(writer);

        if let Err(error) = rotate_files(&self.path, self.backups) {
            self.writer = Some(BufWriter::with_capacity(
                LOG_BUFFER_BYTES,
                open_append(&self.path)?,
            ));
            self.bytes_written = self.path.metadata().map_or(0, |metadata| metadata.len());
            return Err(error);
        }

        self.writer = Some(BufWriter::with_capacity(
            LOG_BUFFER_BYTES,
            open_append(&self.path)?,
        ));
        self.bytes_written = 0;
        Ok(())
    }
}

impl IoWrite for RotatingFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.write_record(buffer)?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer
            .as_mut()
            .expect("rotating log writer must remain open")
            .flush()
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn rotate_files(path: &Path, backups: usize) -> io::Result<()> {
    if backups == 0 {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
    }

    for index in (1..backups).rev() {
        replace_file(&backup_path(path, index), &backup_path(path, index + 1))?;
    }
    replace_file(path, &backup_path(path, 1))
}

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    if !source.exists() {
        return Ok(());
    }
    match fs::remove_file(destination) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(source, destination)
}

fn backup_path(path: &Path, index: usize) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(format!(".{index}"));
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_record(level: log::Level, message: &str) -> QueuedRecord {
        QueuedRecord {
            timestamp: SystemTime::UNIX_EPOCH,
            level,
            target: "test".into(),
            message: message.into(),
        }
    }

    #[test]
    fn saturation_coalesces_low_levels_and_preserves_warning_visibility() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("helix.log");
        let (queue, receiver) = LogQueue::bounded(4, 1).unwrap();

        assert!(queue.try_enqueue(test_record(log::Level::Info, "first")));
        assert!(queue.try_enqueue(test_record(log::Level::Debug, "second")));
        assert!(queue.try_enqueue(test_record(log::Level::Trace, "third")));
        assert!(!queue.try_enqueue_with(log::Level::Trace, || {
            panic!("a saturated producer must not format its record")
        }));
        assert!(!queue.try_enqueue(test_record(log::Level::Debug, "drop-debug")));
        assert!(!queue.try_enqueue(test_record(log::Level::Info, "drop-info")));
        assert!(queue.try_enqueue(test_record(log::Level::Warn, "visible-warning")));
        assert!(!queue.try_enqueue(test_record(log::Level::Error, "overflow-error")));

        let messages = (0..4)
            .map(|_| match receiver.try_recv().unwrap() {
                WriterCommand::Record(record) => record.message.into_string(),
                WriterCommand::Flush(_) | WriterCommand::Shutdown(_) => {
                    panic!("unexpected control message")
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(messages, ["first", "second", "third", "visible-warning"]);

        let mut output = RotatingFile::open(&path, 1_024, 1).unwrap();
        report_dropped(&mut output, &queue.dropped).unwrap();
        output.flush().unwrap();
        let report = fs::read_to_string(path).unwrap();
        assert!(report.contains("dropped trace=1 debug=1 info=1"));
        assert!(report.contains("dropped warning=0 error=1"));
        assert!(!report.contains("overflow-error"));
        assert_eq!(report.matches("log queue saturated").count(), 1);
    }

    #[test]
    fn flush_and_drop_drain_the_writer_queue() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("helix.log");
        let logger = AsyncFileLogger::open_with_config(
            &path,
            log::LevelFilter::Trace,
            AsyncFileLoggerConfig {
                max_bytes: 1_024 * 1_024,
                backups: 1,
                queue_capacity: 8,
                emergency_reserve: 2,
                flush_interval: Duration::from_secs(60),
            },
        )
        .unwrap();

        assert!(logger
            .queue
            .try_enqueue(test_record(log::Level::Info, "before-flush")));
        logger.flush_result().unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("before-flush"));

        assert!(logger
            .queue
            .try_enqueue(test_record(log::Level::Info, "before-drop")));
        drop(logger);

        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("before-drop"));
        assert!(contents.find("before-flush") < contents.find("before-drop"));
    }

    #[test]
    fn rotates_at_the_size_limit_and_retains_bounded_backups() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("helix.log");
        let mut log = RotatingFile::open(&path, 8, 2).unwrap();

        log.write_all(b"12345678").unwrap();
        log.write_all(b"abcdefgh").unwrap();
        log.write_all(b"ABCDEFGH").unwrap();
        log.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"ABCDEFGH");
        assert_eq!(fs::read(backup_path(&path, 1)).unwrap(), b"abcdefgh");
        assert_eq!(fs::read(backup_path(&path, 2)).unwrap(), b"12345678");
        assert!(!backup_path(&path, 3).exists());
    }

    #[test]
    fn rotates_an_oversized_existing_log_on_open() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("helix.log");
        fs::write(&path, b"oversized").unwrap();

        let mut log = RotatingFile::open(&path, 4, 1).unwrap();
        log.write_all(b"new").unwrap();
        log.flush().unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read(backup_path(&path, 1)).unwrap(), b"oversized");
    }
}
