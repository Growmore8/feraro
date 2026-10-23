//! Append-only event journal with deterministic replay.
//!
//! # Invariant P0-3 — the journal is the database
//!
//! The engine writes here and nowhere else on the trade path. PostgreSQL is
//! fed asynchronously from this stream and is never read during matching. That
//! is the single most important difference between this design and the
//! SQL-Server-in-the-trade-path architecture we are replacing.
//!
//! # Frame format
//!
//! ```text
//! ┌────────────┬────────────┬──────────────────┐
//! │ len: u32   │ crc32: u32 │ body: len bytes  │
//! │ little-end │ of body    │ domain::encode() │
//! └────────────┴────────────┴──────────────────┘
//! ```
//!
//! A process killed mid-`append` leaves a short or checksum-failing final
//! frame. [`Journal::open`] truncates that torn tail and resumes from the last
//! intact record, which is why a hard kill costs at most one event rather than
//! the whole file.

#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use domain::{decode, encode, CodecError, Event, Record, Seq, Timestamp};

/// Bytes of frame header preceding every record body.
pub const HEADER_LEN: usize = 8;

/// Largest record body we will read, as a guard against a corrupt length
/// field asking us to allocate gigabytes.
pub const MAX_BODY_LEN: u32 = 1 << 20;

/// Something went wrong reading or writing the journal.
#[derive(Debug)]
pub enum JournalError {
    /// Underlying file error.
    Io(io::Error),
    /// A frame body did not decode.
    Codec(CodecError),
    /// A frame body did not match its checksum.
    Checksum {
        /// Byte offset of the failing frame.
        offset: u64,
    },
    /// Records are not consecutive — a record is missing or duplicated.
    SequenceGap {
        /// Sequence we expected next.
        expected: Seq,
        /// Sequence actually found.
        found: Seq,
    },
    /// A length field exceeded [`MAX_BODY_LEN`].
    FrameTooLarge {
        /// The rejected length.
        len: u32,
    },
    /// A segment number is missing from the sequence on disk.
    MissingSegment {
        /// The segment that should have been there.
        expected: u32,
        /// What was found instead.
        found: u32,
    },
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Io(e) => write!(f, "journal io: {e}"),
            JournalError::Codec(e) => write!(f, "journal decode: {e}"),
            JournalError::Checksum { offset } => {
                write!(f, "checksum mismatch at byte {offset}")
            }
            JournalError::SequenceGap { expected, found } => {
                write!(f, "sequence gap: expected {expected}, found {found}")
            }
            JournalError::FrameTooLarge { len } => {
                write!(f, "frame length {len} exceeds {MAX_BODY_LEN}")
            }
            JournalError::MissingSegment { expected, found } => {
                write!(f, "segment {expected} is missing; found {found} instead")
            }
        }
    }
}

impl std::error::Error for JournalError {}

impl From<io::Error> for JournalError {
    fn from(e: io::Error) -> Self {
        JournalError::Io(e)
    }
}

impl From<CodecError> for JournalError {
    fn from(e: CodecError) -> Self {
        JournalError::Codec(e)
    }
}

/// CRC-32 (IEEE 802.3), table driven.
///
/// Hand-rolled rather than pulled from a crate so the workspace keeps its
/// zero-dependency property — see the note in the workspace manifest.
fn crc32(data: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        let mut i = 0usize;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    });
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = (crc >> 8) ^ table[usize::from((crc as u8) ^ b)];
    }
    !crc
}

/// Result of scanning a journal file for its intact prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanReport {
    /// Byte length of the intact prefix.
    pub good_bytes: u64,
    /// Number of records in that prefix.
    pub records: u64,
    /// Sequence the next append should carry.
    pub next_seq: Seq,
    /// Bytes discarded as a torn tail.
    pub truncated_bytes: u64,
}

/// Splits a byte buffer into intact frames, stopping at the first damaged one.
///
/// Returns the decoded records and the byte offset where scanning stopped.
fn scan(buf: &[u8]) -> (Vec<Record>, u64) {
    let mut records = Vec::new();
    let mut pos = 0usize;

    while pos + HEADER_LEN <= buf.len() {
        let len = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
        if len == 0 || len > MAX_BODY_LEN {
            break;
        }
        let want = u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]);
        let body_start = pos + HEADER_LEN;
        let body_end = body_start + len as usize;
        if body_end > buf.len() {
            break; // torn tail: the body never made it to disk
        }
        let body = &buf[body_start..body_end];
        if crc32(body) != want {
            break; // torn tail: the body made it but is damaged
        }
        match decode(body) {
            Ok(rec) => records.push(rec),
            Err(_) => break,
        }
        pos = body_end;
    }

    (records, pos as u64)
}

/// Confirms records are consecutive from `from`, returning the next sequence.
fn check_gapless(records: &[Record], from: Seq) -> Result<Seq, JournalError> {
    let mut expected = from;
    for rec in records {
        if rec.seq != expected {
            return Err(JournalError::SequenceGap {
                expected,
                found: rec.seq,
            });
        }
        expected = Seq(expected.0 + 1);
    }
    Ok(expected)
}

/// An open, writable journal.
#[derive(Debug)]
pub struct Journal {
    file: File,
    path: PathBuf,
    next_seq: Seq,
    bytes: u64,
    dirty: bool,
    scan: ScanReport,
}

impl Journal {
    /// Opens or creates a journal, repairing a torn tail if one is present.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, JournalError> {
        Journal::open_from(path, Seq(0))
    }

    /// Opens a journal whose first record carries `from` rather than zero.
    ///
    /// This is what makes segmentation possible: sequence numbers run
    /// unbroken across files, so a gap at a segment boundary is as loud an
    /// error as a gap inside one.
    pub fn open_from<P: AsRef<Path>>(path: P, from: Seq) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        let total = buf.len() as u64;

        let (records, good_bytes) = scan(&buf);
        let next_seq = check_gapless(&records, from)?;

        let truncated_bytes = total - good_bytes;
        if truncated_bytes > 0 {
            file.set_len(good_bytes)?;
        }
        file.seek(SeekFrom::Start(good_bytes))?;

        let scan = ScanReport {
            good_bytes,
            records: records.len() as u64,
            next_seq,
            truncated_bytes,
        };

        Ok(Journal {
            file,
            path,
            next_seq,
            bytes: good_bytes,
            dirty: false,
            scan,
        })
    }

    /// What [`open`](Self::open) found on disk, including any repair.
    pub fn scan_report(&self) -> ScanReport {
        self.scan
    }

    /// Path this journal is backed by.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sequence the next appended record will carry.
    pub fn next_seq(&self) -> Seq {
        self.next_seq
    }

    /// Bytes currently on disk.
    ///
    /// Tracked rather than queried so a roll decision costs no syscall on the
    /// trade path.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Appends one event, assigning it the next sequence.
    ///
    /// `at` is supplied by the caller rather than read from the system clock,
    /// so that a replay reproduces identical timestamps.
    pub fn append(&mut self, at: Timestamp, event: Event) -> Result<Record, JournalError> {
        let record = Record {
            seq: self.next_seq,
            at,
            event,
        };
        let body = encode(&record);
        let len =
            u32::try_from(body.len()).map_err(|_| JournalError::FrameTooLarge { len: u32::MAX })?;
        if len > MAX_BODY_LEN {
            return Err(JournalError::FrameTooLarge { len });
        }

        let mut frame = Vec::with_capacity(HEADER_LEN + body.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&crc32(&body).to_le_bytes());
        frame.extend_from_slice(&body);

        // One write call per frame: a partial write is then a torn tail that
        // `open` repairs, never an interleaved half-record.
        self.file.write_all(&frame)?;
        self.bytes += frame.len() as u64;
        self.dirty = true;
        self.next_seq = Seq(self.next_seq.0 + 1);
        Ok(record)
    }

    /// Flushes the OS buffer to durable storage.
    ///
    /// Called on a cadence by the engine, not per event: an fsync per order
    /// caps throughput at the disk's IOPS. The window this opens is bounded by
    /// the flush interval and is a deliberate, documented trade.
    pub fn sync(&mut self) -> Result<(), JournalError> {
        self.file.flush()?;
        self.file.sync_data()?;
        self.dirty = false;
        Ok(())
    }

    /// True when records have been written since the last [`sync`].
    ///
    /// Written is not durable. A record appended here sits in the operating
    /// system's buffer until synced, and a power cut loses it.
    ///
    /// [`sync`]: Self::sync
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
}

/// Reads every record from a journal file, verifying checksums and ordering.
pub fn read_all<P: AsRef<Path>>(path: P) -> Result<Vec<Record>, JournalError> {
    read_all_from(path, Seq(0))
}

/// Reads a file whose first record carries `from` rather than zero.
///
/// A segment other than the first does not start at zero, and checking it
/// against zero would report a sequence gap on every one of them.
pub fn read_all_from<P: AsRef<Path>>(path: P, from: Seq) -> Result<Vec<Record>, JournalError> {
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    let (records, _) = scan(&buf);
    check_gapless(&records, from)?;
    Ok(records)
}

/// Reads every record, refusing to silently skip a damaged frame.
///
/// [`read_all`] stops at a torn tail, which is correct for recovery. This is
/// the stricter form used in CI, where any damage at all should fail the run.
pub fn read_all_strict<P: AsRef<Path>>(path: P) -> Result<Vec<Record>, JournalError> {
    read_all_strict_from(path, Seq(0))
}

/// Strict read of a file whose first record carries `from`.
pub fn read_all_strict_from<P: AsRef<Path>>(
    path: P,
    from: Seq,
) -> Result<Vec<Record>, JournalError> {
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    let (records, good) = scan(&buf);
    if good != buf.len() as u64 {
        return Err(JournalError::Checksum { offset: good });
    }
    check_gapless(&records, from)?;
    Ok(records)
}

// ───────────────────────────────────────────────────────────────────
// Segmentation
// ───────────────────────────────────────────────────────────────────

/// Size at which the active segment is closed and a new one started.
///
/// Small enough that replaying one segment is bounded work and a corrupt
/// segment loses a bounded amount; large enough that rolling is rare. Eight
/// megabytes is roughly four hours of the demo feed.
pub const DEFAULT_SEGMENT_BYTES: u64 = 8 * 1024 * 1024;

/// A journal spread across numbered files.
///
/// # Why one file was never going to be enough
///
/// [`read_all`] loads an entire journal into memory. That is correct and fast
/// for a day and impossible for a year: the demo feed alone writes tens of
/// megabytes a day, and a broker's would write far more. Segments bound both
/// the memory a replay needs and the damage one bad write can do.
///
/// # Only the last segment may be torn
///
/// A process killed mid-append leaves a short final frame in the file it was
/// writing, and [`Journal::open`] repairs that. A *sealed* segment was closed
/// cleanly and can never legitimately be short. So a torn tail in segment 3 of
/// 7 is not a crash to recover from, it is corruption — a truncated backup, a
/// bad disk, someone editing files by hand — and replay refuses it rather than
/// silently serving a portfolio missing four days of trades.
///
/// # What this deliberately does not do
///
/// Snapshotting. Startup still replays every segment, which is bounded memory
/// but not bounded time. Compaction needs a serialised form of engine state,
/// which is its own design decision and its own format to version.
#[derive(Debug)]
pub struct SegmentedJournal {
    dir: PathBuf,
    base: String,
    max_bytes: u64,
    active: Journal,
    active_index: u32,
}

impl SegmentedJournal {
    /// Opens the newest segment, or starts the first one.
    pub fn open<P: AsRef<Path>>(dir: P, base: &str) -> Result<Self, JournalError> {
        Self::open_with(dir, base, DEFAULT_SEGMENT_BYTES)
    }

    /// Opens with a chosen roll size. Tests use a tiny one.
    pub fn open_with<P: AsRef<Path>>(
        dir: P,
        base: &str,
        max_bytes: u64,
    ) -> Result<Self, JournalError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;

        let indices = segment_indices(&dir, base)?;
        let (active_index, from) = match indices.last() {
            None => (1u32, Seq(0)),
            Some(&last) => {
                // Everything before the active segment must be intact, and the
                // sequence must run unbroken up to it.
                let mut expected = Seq(0);
                for index in &indices[..indices.len() - 1] {
                    let path = segment_path(&dir, base, *index);
                    let records = read_all_strict_from(&path, expected)?;
                    expected = check_gapless(&records, expected)?;
                }
                (last, expected)
            }
        };

        let active = Journal::open_from(segment_path(&dir, base, active_index), from)?;
        Ok(SegmentedJournal {
            dir,
            base: base.to_owned(),
            max_bytes,
            active,
            active_index,
        })
    }

    /// Sequence the next appended record will carry.
    pub fn next_seq(&self) -> Seq {
        self.active.next_seq()
    }

    /// What opening the active segment found, including any repair.
    pub fn scan_report(&self) -> ScanReport {
        self.active.scan_report()
    }

    /// The active segment's number.
    pub fn active_index(&self) -> u32 {
        self.active_index
    }

    /// Every segment on disk, oldest first. Archival reads this.
    pub fn segments(&self) -> Result<Vec<PathBuf>, JournalError> {
        Ok(segment_indices(&self.dir, &self.base)?
            .into_iter()
            .map(|i| segment_path(&self.dir, &self.base, i))
            .collect())
    }

    /// Appends, rolling to a new segment first when the active one is full.
    ///
    /// Rolls *before* the write, never in the middle of one, so a segment
    /// boundary can never fall inside a frame.
    pub fn append(&mut self, at: Timestamp, event: Event) -> Result<Record, JournalError> {
        if self.active.bytes() >= self.max_bytes {
            self.roll()?;
        }
        self.active.append(at, event)
    }

    /// Seals the active segment and opens the next.
    pub fn roll(&mut self) -> Result<(), JournalError> {
        self.active.sync()?;
        let next_index = self.active_index + 1;
        let from = self.active.next_seq();
        self.active = Journal::open_from(segment_path(&self.dir, &self.base, next_index), from)?;
        self.active_index = next_index;
        Ok(())
    }

    /// Flushes the active segment to durable storage.
    pub fn sync(&mut self) -> Result<(), JournalError> {
        self.active.sync()
    }

    /// True when the active segment holds records not yet synced.
    ///
    /// A sealed segment is always clean: [`roll`](Self::roll) syncs before it
    /// moves on.
    pub fn is_dirty(&self) -> bool {
        self.active.is_dirty()
    }
}

/// When the engine should force its journal to disk.
///
/// # Two rules, and the second is the one people forget
///
/// A decision — anything [`Event::must_be_durable`] says so about — is synced
/// before anyone is told. That is the obvious rule.
///
/// Quotes are synced on a cadence rather than one by one, because an fsync per
/// tick caps the whole engine at the disk's write rate. But "on a cadence" has
/// to mean a *bounded* window. Without the interval check, a quiet market with
/// no decisions would leave quotes unsynced indefinitely, and the durability
/// window would be however long it happened to be since the last trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Durability {
    /// Longest a written record may stay unsynced when nothing forces it.
    pub interval: std::time::Duration,
}

impl Default for Durability {
    fn default() -> Self {
        Durability {
            interval: std::time::Duration::from_millis(200),
        }
    }
}

impl Durability {
    /// Whether to sync now.
    ///
    /// Pure, so the whole policy can be tested without a disk or a clock.
    pub fn should_sync(
        &self,
        decision_pending: bool,
        dirty: bool,
        since_last_sync: std::time::Duration,
    ) -> bool {
        decision_pending || (dirty && since_last_sync >= self.interval)
    }
}

/// Path of one segment. Six digits so the names sort chronologically.
pub fn segment_path(dir: &Path, base: &str, index: u32) -> PathBuf {
    dir.join(format!("{base}.{index:06}.jnl"))
}

/// Segment numbers present in `dir`, ascending, with holes reported.
fn segment_indices(dir: &Path, base: &str) -> Result<Vec<u32>, JournalError> {
    let prefix = format!("{base}.");
    let mut indices = Vec::new();

    let listing = match std::fs::read_dir(dir) {
        Ok(l) => l,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(indices),
        Err(e) => return Err(JournalError::Io(e)),
    };

    for entry in listing {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(digits) = rest.strip_suffix(".jnl") else {
            continue;
        };
        if let Ok(index) = digits.parse::<u32>() {
            indices.push(index);
        }
    }
    indices.sort_unstable();

    // A missing segment is not something to work around. It means part of the
    // history is gone, and every balance derived from what remains is wrong.
    for (position, index) in indices.iter().enumerate() {
        let expected = position as u32 + 1;
        if *index != expected {
            return Err(JournalError::MissingSegment {
                expected,
                found: *index,
            });
        }
    }
    Ok(indices)
}

/// Streams every record across every segment, in order.
///
/// Holds at most one segment in memory at a time, which is the whole point.
pub struct Replay {
    segments: std::vec::IntoIter<PathBuf>,
    current: std::vec::IntoIter<Record>,
    expected: Seq,
    /// True once the last segment is the one being read, so its torn tail is
    /// tolerated where an earlier segment's would not be.
    remaining: usize,
    failed: bool,
}

impl Replay {
    /// Opens a replay over `dir`.
    pub fn open<P: AsRef<Path>>(dir: P, base: &str) -> Result<Self, JournalError> {
        let dir = dir.as_ref();
        let paths: Vec<PathBuf> = segment_indices(dir, base)?
            .into_iter()
            .map(|i| segment_path(dir, base, i))
            .collect();
        let remaining = paths.len();
        Ok(Replay {
            segments: paths.into_iter(),
            current: Vec::new().into_iter(),
            expected: Seq(0),
            remaining,
            failed: false,
        })
    }

    fn load_next_segment(&mut self) -> Option<Result<(), JournalError>> {
        let path = self.segments.next()?;
        self.remaining -= 1;
        let is_last = self.remaining == 0;

        // A sealed segment must be whole; only the active one may be torn.
        let read = if is_last {
            read_all_from(&path, self.expected)
        } else {
            read_all_strict_from(&path, self.expected)
        };
        match read {
            Ok(records) => {
                self.current = records.into_iter();
                Some(Ok(()))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

impl Iterator for Replay {
    type Item = Result<Record, JournalError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            if let Some(record) = self.current.next() {
                if record.seq != self.expected {
                    self.failed = true;
                    return Some(Err(JournalError::SequenceGap {
                        expected: self.expected,
                        found: record.seq,
                    }));
                }
                self.expected = Seq(self.expected.0 + 1);
                return Some(Ok(record));
            }
            match self.load_next_segment()? {
                Ok(()) => continue,
                Err(e) => {
                    self.failed = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

/// Reads every record across every segment into memory.
///
/// Convenience for tests and small journals. Anything long-lived should use
/// [`Replay`], which is why this is not what the gateway calls.
pub fn read_all_segments<P: AsRef<Path>>(dir: P, base: &str) -> Result<Vec<Record>, JournalError> {
    Replay::open(dir, base)?.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::{AccountId, Currency, InstrumentId, OrderId, OrderType, TimeInForce, UserId};
    use money::{Money, Price, Qty, Side};
    use std::fs;

    /// A scratch directory unique to each test, cleaned on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!("feraro-journal-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).expect("scratch dir");
            Scratch(p)
        }
        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn trading_day() -> Vec<Event> {
        vec![
            Event::AccountOpened {
                account: AccountId(1001),
                currency: Currency::USD,
                leverage: 100,
            },
            Event::FundsDeposited {
                account: AccountId(1001),
                amount: Money::parse("25000.00").unwrap(),
                by: UserId::SYSTEM,
            },
            Event::QuoteTicked {
                instrument: InstrumentId(1),
                bid: Price::parse("1.08530").unwrap(),
                ask: Price::parse("1.08532").unwrap(),
            },
            Event::OrderAccepted {
                order: OrderId(7),
                account: AccountId(1001),
                instrument: InstrumentId(1),
                side: Side::Buy,
                kind: OrderType::Market,
                qty: Qty::parse("100000").unwrap(),
                price: Price::ZERO,
                tif: TimeInForce::ImmediateOrCancel,
                by: UserId::SYSTEM,
            },
            Event::OrderFilled {
                order: OrderId(7),
                instrument: InstrumentId(1),
                side: Side::Buy,
                qty: Qty::parse("100000").unwrap(),
                price: Price::parse("1.08532").unwrap(),
                rate: Price::ONE,
                markup: Price::ZERO,
                commission: Money::ZERO,
            },
        ]
    }

    fn write_day(path: &Path) -> Vec<Record> {
        let mut j = Journal::open(path).expect("open");
        let mut written = Vec::new();
        for (i, ev) in trading_day().into_iter().enumerate() {
            written.push(
                j.append(Timestamp(1_725_000_000_000_000_000 + i as i64), ev)
                    .unwrap(),
            );
        }
        j.sync().unwrap();
        written
    }

    #[test]
    fn appends_then_replays_identically() {
        let s = Scratch::new("replay");
        let p = s.file("a.jnl");
        let written = write_day(&p);
        let read = read_all(&p).expect("replay");
        assert_eq!(written, read);
    }

    #[test]
    fn sequences_are_gapless_and_start_at_zero() {
        let s = Scratch::new("seq");
        let p = s.file("a.jnl");
        write_day(&p);
        let read = read_all(&p).unwrap();
        for (i, rec) in read.iter().enumerate() {
            assert_eq!(rec.seq, Seq(i as u64));
        }
    }

    #[test]
    fn reopening_continues_the_sequence() {
        let s = Scratch::new("reopen");
        let p = s.file("a.jnl");
        write_day(&p);

        let mut j = Journal::open(&p).expect("reopen");
        assert_eq!(j.next_seq(), Seq(5));
        assert_eq!(j.scan_report().records, 5);
        assert_eq!(j.scan_report().truncated_bytes, 0);

        j.append(
            Timestamp(1_725_000_000_000_000_100),
            Event::MarginCallIssued {
                account: AccountId(1001),
                margin_level_bp: 6_000,
            },
        )
        .unwrap();
        j.sync().unwrap();

        let read = read_all(&p).unwrap();
        assert_eq!(read.len(), 6);
        assert_eq!(read[5].seq, Seq(5));
    }

    #[test]
    fn replay_is_deterministic_across_runs() {
        // The CI gate: replaying the same journal twice must produce the same
        // records, byte for byte, on any machine.
        let s = Scratch::new("determinism");
        let p = s.file("a.jnl");
        write_day(&p);

        let first: Vec<Vec<u8>> = read_all(&p).unwrap().iter().map(domain::encode).collect();
        let second: Vec<Vec<u8>> = read_all(&p).unwrap().iter().map(domain::encode).collect();
        assert_eq!(first, second);
    }

    #[test]
    fn a_torn_tail_is_repaired_and_costs_only_the_last_event() {
        let s = Scratch::new("torn");
        let p = s.file("a.jnl");
        write_day(&p);
        let full_len = fs::metadata(&p).unwrap().len();

        // Simulate a kill part-way through writing the sixth frame.
        let mut bytes = fs::read(&p).unwrap();
        bytes.extend_from_slice(&[0x20, 0x00, 0x00, 0x00, 0xDE, 0xAD, 0xBE]);
        fs::write(&p, &bytes).unwrap();

        let j = Journal::open(&p).expect("open repairs");
        let report = j.scan_report();
        assert_eq!(report.records, 5);
        assert_eq!(report.next_seq, Seq(5));
        assert_eq!(report.truncated_bytes, 7);
        assert_eq!(fs::metadata(&p).unwrap().len(), full_len);
        assert_eq!(read_all(&p).unwrap().len(), 5);
    }

    #[test]
    fn a_flipped_bit_in_a_body_is_caught_by_the_checksum() {
        let s = Scratch::new("bitflip");
        let p = s.file("a.jnl");
        write_day(&p);

        let mut bytes = fs::read(&p).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        fs::write(&p, &bytes).unwrap();

        // Strict mode refuses the file outright.
        assert!(matches!(
            read_all_strict(&p),
            Err(JournalError::Checksum { .. })
        ));
        // Recovery mode keeps the intact prefix and drops the damaged record.
        assert_eq!(read_all(&p).unwrap().len(), 4);
    }

    #[test]
    fn an_absurd_length_field_does_not_allocate() {
        let s = Scratch::new("huge");
        let p = s.file("a.jnl");
        write_day(&p);

        let mut bytes = fs::read(&p).unwrap();
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 4]);
        fs::write(&p, &bytes).unwrap();

        let j = Journal::open(&p).expect("open");
        assert_eq!(j.scan_report().records, 5);
        assert_eq!(j.scan_report().truncated_bytes, 8);
    }

    #[test]
    fn an_empty_file_opens_clean() {
        let s = Scratch::new("empty");
        let p = s.file("fresh.jnl");
        let j = Journal::open(&p).expect("create");
        assert_eq!(j.next_seq(), Seq(0));
        assert_eq!(j.scan_report().records, 0);
        assert_eq!(read_all(&p).unwrap(), Vec::new());
    }

    #[test]
    fn crc32_matches_the_known_ieee_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0x0000_0000);
    }

    #[test]
    fn a_sequence_gap_is_reported_not_ignored() {
        let records = vec![
            Record {
                seq: Seq(0),
                at: Timestamp(0),
                event: trading_day()[0],
            },
            Record {
                seq: Seq(2),
                at: Timestamp(1),
                event: trading_day()[0],
            },
        ];
        assert!(matches!(
            check_gapless(&records, Seq(0)),
            Err(JournalError::SequenceGap {
                expected: Seq(1),
                found: Seq(2)
            })
        ));
    }
    // ── Segmentation ────────────────────────────────────────────────

    /// Writes `n` events into a segmented journal that rolls every `bytes`.
    fn fill_segments(dir: &Path, base: &str, n: u64, bytes: u64) -> SegmentedJournal {
        let mut j = SegmentedJournal::open_with(dir, base, bytes).expect("open");
        for i in 0..n {
            j.append(
                Timestamp(i as i64),
                Event::MarginCallIssued {
                    account: AccountId(1),
                    margin_level_bp: i as i64,
                },
            )
            .expect("append");
        }
        j.sync().expect("sync");
        j
    }

    #[test]
    fn a_segmented_journal_rolls_and_keeps_one_unbroken_sequence() {
        let s = Scratch::new("seg-roll");
        // A MarginCallIssued frame is 43 bytes, so 200 bytes is ~4 per segment.
        let j = fill_segments(&s.0, "feraro", 20, 200);

        let segments = j.segments().expect("lists");
        assert!(
            segments.len() > 3,
            "expected several segments, got {}",
            segments.len()
        );

        let records = read_all_segments(&s.0, "feraro").expect("replays");
        assert_eq!(records.len(), 20);
        for (i, r) in records.iter().enumerate() {
            assert_eq!(
                r.seq,
                Seq(i as u64),
                "sequence must not restart per segment"
            );
        }
    }

    #[test]
    fn reopening_continues_into_the_active_segment() {
        let s = Scratch::new("seg-reopen");
        fill_segments(&s.0, "feraro", 20, 200);

        let mut j = SegmentedJournal::open_with(&s.0, "feraro", 200).expect("reopen");
        assert_eq!(j.next_seq(), Seq(20));
        j.append(
            Timestamp(999),
            Event::MarginCallIssued {
                account: AccountId(1),
                margin_level_bp: 999,
            },
        )
        .unwrap();
        j.sync().unwrap();

        let records = read_all_segments(&s.0, "feraro").unwrap();
        assert_eq!(records.len(), 21);
        assert_eq!(records[20].seq, Seq(20));
    }

    #[test]
    fn a_torn_tail_in_the_active_segment_is_repaired() {
        let s = Scratch::new("seg-torn-active");
        let j = fill_segments(&s.0, "feraro", 20, 200);
        let last = j.segments().unwrap().last().unwrap().clone();
        let before = read_all_segments(&s.0, "feraro").unwrap().len();

        // A process killed mid-append leaves exactly this.
        let mut bytes = fs::read(&last).unwrap();
        bytes.extend_from_slice(&[0x2B, 0x00, 0x00, 0x00, 0xDE, 0xAD]);
        fs::write(&last, &bytes).unwrap();

        let records = read_all_segments(&s.0, "feraro").expect("the tail is repairable");
        assert_eq!(records.len(), before, "no committed record was lost");
    }

    #[test]
    fn a_torn_tail_in_a_sealed_segment_is_corruption_and_is_refused() {
        // The distinction this whole design turns on. A sealed segment was
        // closed cleanly and can never legitimately be short, so shortening it
        // means the history is damaged. Serving the surviving prefix would hand
        // back a portfolio missing every trade after the damage, silently.
        let s = Scratch::new("seg-torn-sealed");
        let j = fill_segments(&s.0, "feraro", 20, 200);
        let segments = j.segments().unwrap();
        assert!(segments.len() >= 3);

        let sealed = &segments[1];
        let mut bytes = fs::read(sealed).unwrap();
        bytes.truncate(bytes.len() - 5);
        fs::write(sealed, &bytes).unwrap();

        let err = read_all_segments(&s.0, "feraro").expect_err("must refuse");
        assert!(
            matches!(err, JournalError::Checksum { .. }),
            "expected a checksum error, got {err:?}"
        );
    }

    #[test]
    fn a_missing_segment_is_refused_rather_than_skipped() {
        let s = Scratch::new("seg-hole");
        let j = fill_segments(&s.0, "feraro", 20, 200);
        let segments = j.segments().unwrap();
        assert!(segments.len() >= 3);

        fs::remove_file(&segments[1]).unwrap();

        let err = read_all_segments(&s.0, "feraro").expect_err("must refuse");
        assert!(
            matches!(err, JournalError::MissingSegment { .. }),
            "expected a missing-segment error, got {err:?}"
        );
    }

    #[test]
    fn replay_holds_one_segment_at_a_time() {
        // The reason segments exist. `read_all` on a year of journal would
        // allocate the year; this allocates one segment.
        let s = Scratch::new("seg-stream");
        fill_segments(&s.0, "feraro", 40, 200);

        let mut replay = Replay::open(&s.0, "feraro").unwrap();
        let first = replay.next().expect("a record").expect("ok");
        assert_eq!(first.seq, Seq(0));

        let rest: Result<Vec<_>, _> = replay.collect();
        assert_eq!(rest.unwrap().len(), 39);
    }

    #[test]
    fn a_sequence_gap_across_a_boundary_stops_the_replay() {
        let s = Scratch::new("seg-gap");
        fill_segments(&s.0, "feraro", 8, 200);

        // Rewrite the second segment starting five sequences too late.
        let first = read_all(segment_path(&s.0, "feraro", 1)).unwrap();
        let start = Seq(first.len() as u64);
        let path = segment_path(&s.0, "feraro", 2);
        let existing = read_all_from(&path, start).unwrap();
        fs::remove_file(&path).unwrap();
        let mut forged = Journal::open_from(&path, Seq(start.0 + 5)).unwrap();
        for r in &existing {
            forged.append(r.at, r.event).unwrap();
        }
        forged.sync().unwrap();

        let err = read_all_segments(&s.0, "feraro").expect_err("must refuse");
        assert!(
            matches!(err, JournalError::SequenceGap { .. }),
            "expected a sequence gap, got {err:?}"
        );
    }

    #[test]
    fn an_empty_directory_replays_to_nothing() {
        let s = Scratch::new("seg-empty");
        assert_eq!(read_all_segments(&s.0, "feraro").unwrap(), Vec::new());

        let j = SegmentedJournal::open(&s.0, "feraro").unwrap();
        assert_eq!(j.next_seq(), Seq(0));
        assert_eq!(j.active_index(), 1);
    }

    #[test]
    fn segment_names_sort_chronologically() {
        // Zero padding, so segment 9 comes before segment 10 in a directory
        // listing as well as numerically.
        let dir = Path::new("/tmp");
        let names: Vec<String> = [1u32, 2, 9, 10, 100]
            .iter()
            .map(|i| {
                segment_path(dir, "feraro", *i)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "lexical order must match numeric order");
        assert_eq!(names[0], "feraro.000001.jnl");
    }

    #[test]
    fn files_that_are_not_segments_are_ignored() {
        let s = Scratch::new("seg-noise");
        fill_segments(&s.0, "feraro", 8, 200);
        fs::write(s.0.join("notes.txt"), b"hello").unwrap();
        fs::write(s.0.join("feraro.jnl"), b"an old single-file journal").unwrap();
        fs::write(s.0.join("other.000001.jnl"), b"a different journal").unwrap();

        let records = read_all_segments(&s.0, "feraro").expect("still replays");
        assert_eq!(records.len(), 8);
    }
    // ── Durability ──────────────────────────────────────────────────

    #[test]
    fn a_journal_is_dirty_after_a_write_and_clean_after_a_sync() {
        let s = Scratch::new("dirty");
        let mut j = Journal::open(s.file("a.jnl")).unwrap();
        assert!(
            !j.is_dirty(),
            "a freshly opened journal has nothing unsynced"
        );

        j.append(Timestamp(0), trading_day()[0]).unwrap();
        assert!(j.is_dirty(), "written is not durable");

        j.sync().unwrap();
        assert!(!j.is_dirty());
    }

    #[test]
    fn a_decision_always_forces_a_sync() {
        let d = Durability::default();
        // Even a microsecond after the last sync, even with nothing else dirty.
        assert!(d.should_sync(true, false, std::time::Duration::ZERO));
        assert!(d.should_sync(true, true, std::time::Duration::ZERO));
    }

    #[test]
    fn quotes_wait_for_the_interval_and_no_longer() {
        let d = Durability {
            interval: std::time::Duration::from_millis(200),
        };
        let ms = std::time::Duration::from_millis;
        assert!(!d.should_sync(false, true, ms(0)));
        assert!(!d.should_sync(false, true, ms(199)));
        assert!(d.should_sync(false, true, ms(200)), "the window is bounded");
        assert!(d.should_sync(false, true, ms(5_000)));
    }

    #[test]
    fn nothing_written_means_nothing_to_sync() {
        let d = Durability::default();
        let hour = std::time::Duration::from_secs(3_600);
        assert!(!d.should_sync(false, false, hour));
    }

    #[test]
    fn rolling_leaves_the_sealed_segment_synced() {
        let s = Scratch::new("seg-dirty");
        let mut j = SegmentedJournal::open_with(&s.0, "feraro", 100).unwrap();
        for i in 0..6 {
            j.append(
                Timestamp(i),
                Event::MarginCallIssued {
                    account: AccountId(1),
                    margin_level_bp: i,
                },
            )
            .unwrap();
        }
        assert!(j.active_index() > 1, "expected at least one roll");
        // The active segment has unsynced writes; the sealed ones were synced
        // by `roll`, and replay proves they are whole.
        assert!(j.is_dirty());
        j.sync().unwrap();
        assert!(!j.is_dirty());
        assert_eq!(read_all_segments(&s.0, "feraro").unwrap().len(), 6);
    }
}
