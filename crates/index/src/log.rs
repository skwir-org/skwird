//! The log (`docs/DESIGN.md`, decision 8): every change the index takes,
//! numbered, appended to a file of its own. One structure does three
//! jobs: saves are incremental (the chunks saved hold the changes up to a
//! number, and the log holds the rest), a crash is replayed from it, and
//! it answers `changes {root, since}`, the journal.
//!
//! A record says what one folder now is: its path, facts and state, and
//! the changes to its entries, each with the entry's identity and kind
//! (for a removal, what it was). A refresh that completes adds a record of
//! when the names were proven.
//!
//! A log file, little-endian: `SKWL`, its version (4 bytes), its epoch
//! (8), the number of its first record (8); then records, each its
//! payload's length (4), a checksum of the payload (8), and the payload.
//! A record that does not read whole, or whose checksum differs, ends the
//! log there: a crash cut it short, and what follows is let go of.
//!
//! A token, which a client keeps to ask what changed since, is an epoch
//! and a number, opaque to it. A rebuilt index, a log that could not be
//! written, and a purge start another epoch: a token from another is too
//! old, never answered with what it missed.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use skwird_crawl::{Attrs, Entry, FileId, Kind, Time};

use crate::block::{Facts, State};
use crate::chunk::{kind_code, state_code, state_of};

const MAGIC: &[u8; 4] = b"SKWL";
const VERSION: u32 = 1;
const HEAD: usize = 4 + 4 + 8 + 8;

/// What changed of an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Added,
    Removed,
    /// Its kind, identity or attributes.
    Changed,
}

/// A change to one entry of a folder. For a removal, `entry` is what the
/// entry was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub op: Op,
    pub name: Vec<u8>,
    pub entry: Entry,
}

/// What one folder now is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderRecord {
    /// Its path below the index's root.
    pub path: PathBuf,
    pub facts: Option<Facts>,
    pub state: State,
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    Folder(FolderRecord),
    /// A refresh completed: every folder was compared at this time, or
    /// read after it.
    Proven(Time),
}

/// A record, as the log holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Logged {
    pub seq: u64,
    pub at: Time,
    pub record: Record,
}

/// Where a client is in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub epoch: u64,
    pub seq: u64,
}

impl Token {
    /// The token as a client keeps it.
    pub fn text(self) -> String {
        format!("{:016x}.{}", self.epoch, self.seq)
    }

    pub fn parse(text: &str) -> Option<Token> {
        let (epoch, seq) = text.split_once('.')?;
        Some(Token {
            epoch: u64::from_str_radix(epoch, 16).ok()?,
            seq: seq.parse().ok()?,
        })
    }
}

/// FNV-1a over the payload: a record cut short or torn is told, not a
/// record forged.
fn checksum(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn time(&mut self, t: Time) {
        self.0.extend_from_slice(&t.sec.to_le_bytes());
        self.u32(t.nsec);
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    fn entry(&mut self, e: &Entry) {
        self.u8(kind_code(e.kind) as u8);
        self.u64(e.id.dev);
        self.u64(e.id.ino);
        match e.attrs {
            None => self.u8(0),
            Some(a) => {
                // 2: with its count of names; 1, before it was kept, reads
                // as none known.
                self.u8(2);
                self.u64(a.size);
                self.u64(a.alloc);
                self.time(a.mtime);
                self.time(a.ctime);
                self.u32(a.links);
            }
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let part = self.bytes.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(part)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn time(&mut self) -> Option<Time> {
        Some(Time {
            sec: i64::from_le_bytes(self.take(8)?.try_into().ok()?),
            nsec: self.u32()?,
        })
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        self.take(n).map(<[u8]>::to_vec)
    }
    fn entry(&mut self) -> Option<Entry> {
        let kind = match self.u8()? {
            0 => Kind::File,
            1 => Kind::Folder,
            2 => Kind::Link,
            3 => Kind::Other,
            _ => return None,
        };
        let id = FileId {
            dev: self.u64()?,
            ino: self.u64()?,
        };
        let attrs = match self.u8()? {
            0 => None,
            tag @ (1 | 2) => Some(Attrs {
                size: self.u64()?,
                alloc: self.u64()?,
                mtime: self.time()?,
                ctime: self.time()?,
                links: if tag == 2 { self.u32()? } else { 0 },
            }),
            _ => return None,
        };
        Some(Entry { kind, id, attrs })
    }
}

/// A path's bytes, or a name's: as the system names it on Unix, its WTF-8
/// on Windows ([`skwird_crawl::wtf8`]), as the crawl gives names. Never
/// Rust's encoded bytes of an `OsStr`, which it does not promise.
#[cfg(unix)]
pub(crate) fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    std::borrow::Cow::Borrowed(path.as_os_str().as_bytes())
}

#[cfg(windows)]
pub(crate) fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    let mut out = Vec::new();
    skwird_crawl::wtf8::push(path, &mut out);
    std::borrow::Cow::Owned(out)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn path_bytes(path: &Path) -> std::borrow::Cow<'_, [u8]> {
    std::borrow::Cow::Borrowed(path.as_os_str().as_encoded_bytes())
}

#[cfg(unix)]
fn path_of(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(windows)]
// Unix's takes the bytes whole; the callers are the same on every system.
#[allow(clippy::needless_pass_by_value)]
fn path_of(bytes: Vec<u8>) -> PathBuf {
    skwird_crawl::wtf8::path(&bytes)
}

#[cfg(not(any(unix, windows)))]
fn path_of(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(
        String::from_utf8(bytes)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()),
    )
}

/// A record's payload.
fn encode(logged: &Logged) -> Vec<u8> {
    let mut w = Writer(Vec::new());
    w.u64(logged.seq);
    w.time(logged.at);
    match &logged.record {
        Record::Proven(at) => {
            w.u8(2);
            w.time(*at);
        }
        Record::Folder(f) => {
            w.u8(1);
            w.bytes(&path_bytes(&f.path));
            match f.facts {
                None => w.u8(0),
                Some(facts) => {
                    w.u8(1);
                    w.u64(facts.id.dev);
                    w.u64(facts.id.ino);
                    w.time(facts.ctime);
                    w.time(facts.mtime);
                    w.u32(facts.owner);
                    w.u32(facts.group);
                    w.u16(facts.mode);
                }
            }
            let (code, errno) = state_code(f.state);
            w.u8(code as u8);
            w.u32(errno as u32);
            w.u32(f.changes.len() as u32);
            for c in &f.changes {
                w.u8(match c.op {
                    Op::Added => 0,
                    Op::Removed => 1,
                    Op::Changed => 2,
                });
                w.bytes(&c.name);
                w.entry(&c.entry);
            }
        }
    }
    w.0
}

fn decode(payload: &[u8]) -> Option<Logged> {
    let mut r = Reader {
        bytes: payload,
        at: 0,
    };
    let seq = r.u64()?;
    let at = r.time()?;
    let record = match r.u8()? {
        2 => Record::Proven(r.time()?),
        1 => {
            let path = path_of(r.bytes()?);
            let facts = match r.u8()? {
                0 => None,
                1 => Some(Facts {
                    id: FileId {
                        dev: r.u64()?,
                        ino: r.u64()?,
                    },
                    ctime: r.time()?,
                    mtime: r.time()?,
                    owner: r.u32()?,
                    group: r.u32()?,
                    mode: r.u16()?,
                }),
                _ => return None,
            };
            let code = r.u8()?;
            let errno = r.u32()?;
            let state = state_of(u64::from(code), u64::from(errno))?;
            let count = r.u32()? as usize;
            let mut changes = Vec::with_capacity(count.min(1 << 16));
            for _ in 0..count {
                let op = match r.u8()? {
                    0 => Op::Added,
                    1 => Op::Removed,
                    2 => Op::Changed,
                    _ => return None,
                };
                changes.push(Change {
                    op,
                    name: r.bytes()?,
                    entry: r.entry()?,
                });
            }
            Record::Folder(FolderRecord {
                path,
                facts,
                state,
                changes,
            })
        }
        _ => return None,
    };
    (r.at == payload.len()).then_some(Logged { seq, at, record })
}

/// The bytes `record` takes in a log, its frame with it: what the
/// journal's 16 MB of log are counted in.
pub fn size_of(record: &Record) -> usize {
    let logged = Logged {
        seq: 0,
        at: Time::default(),
        record: record.clone(),
    };
    12 + encode(&logged).len()
}

/// A log file of one epoch, open to append.
pub struct Log {
    file: File,
    path: PathBuf,
    epoch: u64,
    /// The number of its first record.
    first: u64,
    /// The number the next record takes.
    next: u64,
    /// Records appended and not yet made durable.
    unsynced: bool,
}

/// What a log file holds, read whole.
pub struct Contents {
    pub epoch: u64,
    pub first: u64,
    pub records: Vec<Logged>,
    /// Where the records that read whole end: a crash cut what follows.
    pub end: u64,
}

pub fn file_name(epoch: u64) -> String {
    format!("log-{epoch:016x}")
}

impl Log {
    /// A new log file in `dir` for `epoch`, its first record numbered
    /// `first`, its user's alone; a file there of that name is replaced.
    pub fn create(dir: &Path, epoch: u64, first: u64) -> std::io::Result<Log> {
        let path = dir.join(file_name(epoch));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        let mut head = Vec::with_capacity(HEAD);
        head.extend_from_slice(MAGIC);
        head.extend_from_slice(&VERSION.to_le_bytes());
        head.extend_from_slice(&epoch.to_le_bytes());
        head.extend_from_slice(&first.to_le_bytes());
        file.write_all(&head)?;
        file.sync_all()?;
        Ok(Log {
            file,
            path,
            epoch,
            first,
            next: first,
            unsynced: false,
        })
    }

    /// Every record of the log file at `path` that reads whole, in order.
    pub fn read(path: &Path) -> std::io::Result<Option<Contents>> {
        let bytes = std::fs::read(path)?;
        let Some(head) = bytes.get(..HEAD) else {
            return Ok(None);
        };
        if &head[..4] != MAGIC || head[4..8] != VERSION.to_le_bytes() {
            return Ok(None);
        }
        let word = |at: usize| u64::from_le_bytes(head[at..at + 8].try_into().unwrap_or_default());
        let (epoch, first) = (word(8), word(16));
        let mut records = Vec::new();
        let mut at = HEAD;
        let mut expect = first;
        while let Some(len) = bytes.get(at..at + 4) {
            let len = u32::from_le_bytes(len.try_into().unwrap_or_default()) as usize;
            let Some(sum) = bytes.get(at + 4..at + 12) else {
                break;
            };
            let Some(payload) = bytes.get(at + 12..at + 12 + len) else {
                break;
            };
            if u64::from_le_bytes(sum.try_into().unwrap_or_default()) != checksum(payload) {
                break;
            }
            let Some(logged) = decode(payload).filter(|l| l.seq == expect) else {
                break;
            };
            records.push(logged);
            expect += 1;
            at += 12 + len;
        }
        Ok(Some(Contents {
            epoch,
            first,
            records,
            end: at as u64,
        }))
    }

    /// The log file at `path`, open to append after its records that read
    /// whole: what a crash left past them is cut off.
    pub fn reopen(path: &Path, read: &Contents) -> std::io::Result<Log> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        file.set_len(read.end)?;
        file.seek(SeekFrom::End(0))?;
        file.sync_all()?;
        Ok(Log {
            file,
            path: path.to_path_buf(),
            epoch: read.epoch,
            first: read.first,
            next: read.first + read.records.len() as u64,
            unsynced: false,
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn first(&self) -> u64 {
        self.first
    }

    /// The number the next record takes.
    pub fn next(&self) -> u64 {
        self.next
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append `record`, numbered, as of `at`; it is durable once
    /// [`Log::sync`] returns.
    pub fn append(&mut self, at: Time, record: Record) -> std::io::Result<u64> {
        let seq = self.next;
        let payload = encode(&Logged { seq, at, record });
        let mut bytes = Vec::with_capacity(12 + payload.len());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&checksum(&payload).to_le_bytes());
        bytes.extend_from_slice(&payload);
        self.file.write_all(&bytes)?;
        self.next += 1;
        self.unsynced = true;
        Ok(seq)
    }

    /// Make every record appended durable.
    pub fn sync(&mut self) -> std::io::Result<()> {
        if self.unsynced {
            self.file.sync_data()?;
            self.unsynced = false;
        }
        Ok(())
    }

    /// The token of what the log holds now.
    pub fn token(&self) -> Token {
        Token {
            epoch: self.epoch,
            seq: self.next.saturating_sub(1),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use skwird_crawl::Why;
    use skwird_testkit::Scratch;

    fn sample() -> Vec<Record> {
        let entry = Entry {
            kind: Kind::File,
            id: FileId { dev: 3, ino: 99 },
            attrs: Some(Attrs {
                size: 12,
                alloc: 4096,
                mtime: Time { sec: -5, nsec: 1 },
                ctime: Time { sec: 7, nsec: 2 },
                links: 2,
            }),
        };
        vec![
            Record::Folder(FolderRecord {
                path: PathBuf::from("a/été"),
                facts: Some(Facts {
                    id: FileId { dev: 3, ino: 256 },
                    ctime: Time { sec: 1, nsec: 2 },
                    mtime: Time { sec: 3, nsec: 4 },
                    owner: 1000,
                    group: 100,
                    mode: 0o755,
                }),
                state: State::Listed { again: true },
                changes: vec![
                    Change {
                        op: Op::Added,
                        name: b"new \xff".to_vec(),
                        entry,
                    },
                    Change {
                        op: Op::Removed,
                        name: b"old".to_vec(),
                        entry: Entry {
                            attrs: None,
                            ..entry
                        },
                    },
                ],
            }),
            Record::Proven(Time { sec: 9, nsec: 9 }),
            Record::Folder(FolderRecord {
                path: PathBuf::from("b"),
                facts: None,
                state: State::NotCovered(Why::CutShort(5)),
                changes: Vec::new(),
            }),
        ]
    }

    #[test]
    fn records_come_back_in_order_as_they_were_appended() {
        let s = Scratch::new("log");
        let mut log = Log::create(s.path(), 0xabc, 10).unwrap();
        for (i, r) in (0..).zip(sample()) {
            assert_eq!(
                log.append(Time { sec: i, nsec: 0 }, r).unwrap(),
                10 + i.unsigned_abs()
            );
        }
        log.sync().unwrap();
        assert_eq!(log.token().text(), "0000000000000abc.12");
        let read = Log::read(log.path()).unwrap().unwrap();
        assert_eq!((read.epoch, read.first), (0xabc, 10));
        let records: Vec<Record> = read.records.into_iter().map(|l| l.record).collect();
        assert_eq!(records, sample());
        assert_eq!(Token::parse("0000000000000abc.12"), Some(log.token()));
    }

    /// A record cut short ends the log there; reopened, the log appends
    /// after the last record that reads whole.
    #[test]
    fn a_torn_tail_ends_the_log_and_is_cut_off() {
        let s = Scratch::new("log-torn");
        let mut log = Log::create(s.path(), 1, 0).unwrap();
        for r in sample() {
            log.append(Time::default(), r).unwrap();
        }
        log.sync().unwrap();
        let path = log.path().to_path_buf();
        drop(log);
        let whole = std::fs::read(&path).unwrap();
        for cut in [whole.len() - 1, whole.len() - 20, HEAD + 5] {
            std::fs::write(&path, &whole[..cut]).unwrap();
            let read = Log::read(&path).unwrap().unwrap();
            assert!(read.records.len() < 3, "cut at {cut}");
            let mut log = Log::reopen(&path, &read).unwrap();
            let kept = read.records.len() as u64;
            assert_eq!(log.next(), kept);
            log.append(Time::default(), Record::Proven(Time::default()))
                .unwrap();
            log.sync().unwrap();
            let again = Log::read(&path).unwrap().unwrap();
            assert_eq!(
                again.records.len() as u64,
                kept + 1,
                "appended after the tear"
            );
        }
        // A flipped byte inside a record ends the log at it.
        let mut flipped = whole.clone();
        flipped[HEAD + 14] ^= 0xff;
        std::fs::write(&path, &flipped).unwrap();
        let kept = Log::read(&path).unwrap().unwrap().records;
        assert!(kept.is_empty(), "{kept:?}");
    }
}
