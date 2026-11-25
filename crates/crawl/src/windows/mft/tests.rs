use std::collections::BTreeMap;
use std::path::Path;

use skwird_testkit::{Kind, Scratch};

use super::*;

const RECORD: usize = 1024;
/// 1970, as a file time.
const EPOCH: i64 = 116_444_736_000_000_000;

/// A resident attribute of `kind`, named `name`, holding `value`.
fn resident(kind: u32, name: &str, value: &[u8]) -> Vec<u8> {
    let wide: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let value_at = (0x18 + wide.len()).next_multiple_of(8);
    let mut a = vec![0u8; value_at];
    a[0..4].copy_from_slice(&kind.to_le_bytes());
    a[9] = u8::try_from(name.encode_utf16().count()).unwrap();
    a[0x0A..0x0C].copy_from_slice(&0x18u16.to_le_bytes());
    a[0x10..0x14].copy_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
    a[0x14..0x16].copy_from_slice(&u16::try_from(value_at).unwrap().to_le_bytes());
    a[0x18..0x18 + wide.len()].copy_from_slice(&wide);
    a.extend_from_slice(value);
    a.resize(a.len().next_multiple_of(8), 0);
    let len = u32::try_from(a.len()).unwrap();
    a[4..8].copy_from_slice(&len.to_le_bytes());
    a
}

/// The unnamed `$DATA`, non-resident from `vcn`, its runs `runs`.
fn nonresident(vcn: u64, size: u64, alloc: u64, runs: &[u8]) -> Vec<u8> {
    let mut a = vec![0u8; 0x40];
    a[0..4].copy_from_slice(&DATA.to_le_bytes());
    a[8] = 1;
    a[0x0A..0x0C].copy_from_slice(&0x40u16.to_le_bytes());
    a[0x10..0x18].copy_from_slice(&vcn.to_le_bytes());
    a[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
    a[0x28..0x30].copy_from_slice(&alloc.to_le_bytes());
    a[0x30..0x38].copy_from_slice(&size.to_le_bytes());
    a[0x38..0x40].copy_from_slice(&size.to_le_bytes());
    a.extend_from_slice(runs);
    a.push(0);
    a.resize(a.len().next_multiple_of(8), 0);
    let len = u32::try_from(a.len()).unwrap();
    a[4..8].copy_from_slice(&len.to_le_bytes());
    a
}

fn standard(mtime: i64, ctime: i64, attributes: u32, security: u32) -> Vec<u8> {
    let mut v = vec![0u8; 0x48];
    v[0x08..0x10].copy_from_slice(&mtime.to_le_bytes());
    v[0x10..0x18].copy_from_slice(&ctime.to_le_bytes());
    v[0x20..0x24].copy_from_slice(&attributes.to_le_bytes());
    v[0x34..0x38].copy_from_slice(&security.to_le_bytes());
    resident(STANDARD_INFORMATION, "", &v)
}

fn file_name(parent: Reference, name: &str, namespace: u8) -> Vec<u8> {
    let wide: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut v = vec![0u8; 0x42];
    v[0..8].copy_from_slice(&parent.to_le_bytes());
    v[0x40] = u8::try_from(name.encode_utf16().count()).unwrap();
    v[0x41] = namespace;
    v.extend_from_slice(&wide);
    resident(FILE_NAME, "", &v)
}

/// A record as NTFS writes it: its head, its attributes, then each
/// sector's last two bytes moved into its update sequence array and
/// replaced by its update number.
fn record(sequence: u16, flags: u16, base: Reference, attributes: &[Vec<u8>]) -> Vec<u8> {
    let mut r = vec![0u8; RECORD];
    r[0..4].copy_from_slice(b"FILE");
    let sectors = RECORD / STRIDE;
    r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
    r[6..8].copy_from_slice(&u16::try_from(sectors + 1).unwrap().to_le_bytes());
    r[0x10..0x12].copy_from_slice(&sequence.to_le_bytes());
    let first = (0x30 + 2 * (sectors + 1)).next_multiple_of(8);
    r[0x14..0x16].copy_from_slice(&u16::try_from(first).unwrap().to_le_bytes());
    r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
    r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
    let mut at = first;
    for a in attributes {
        r[at..at + a.len()].copy_from_slice(a);
        at += a.len();
    }
    r[at..at + 4].copy_from_slice(&END.to_le_bytes());
    let used = u32::try_from(at + 8).unwrap();
    r[0x18..0x1C].copy_from_slice(&used.to_le_bytes());
    r[0x1C..0x20].copy_from_slice(&u32::try_from(RECORD).unwrap().to_le_bytes());
    let usn = [0x2A, 0x00];
    r[0x30..0x32].copy_from_slice(&usn);
    for i in 0..sectors {
        let end = (i + 1) * STRIDE;
        let hidden = [r[end - 2], r[end - 1]];
        r[0x32 + 2 * i..0x34 + 2 * i].copy_from_slice(&hidden);
        r[end - 2..end].copy_from_slice(&usn);
    }
    r
}

const IN_USE: u16 = 1;
const FOLDER: u16 = 3;

fn reference(number: u64, sequence: u16) -> Reference {
    number | (u64::from(sequence) << 48)
}

fn wtf8(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    crate::wtf8::encode(name.encode_utf16(), &mut out);
    out
}

/// A record reads back as written: its times, attributes and security id;
/// its long names, not its short one; its data's size, resident; and a
/// sector's last bytes restored, where one of its names crosses it.
#[test]
fn a_record_reads_back_as_written() {
    let long = "un nom assez long pour couvrir la fin du premier secteur, été 東京";
    let mut padding = String::new();
    // 160 characters: its attribute ends past the first sector's end.
    for _ in 0..5 {
        padding.push_str("0123456789abcdef0123456789abcdef");
    }
    let mut r = record(
        7,
        IN_USE,
        0,
        &[
            standard(EPOCH + 10_000_000, EPOCH + 20_000_000, 0x20, 261),
            file_name(reference(40, 2), &padding, 1),
            file_name(reference(40, 2), long, 1),
            file_name(reference(40, 2), "UNNOMA~1", DOS),
            resident(DATA, "", b"hello"),
        ],
    );
    let d = decode(41, &mut r).unwrap();
    assert_eq!((d.number, d.sequence, d.base, d.folder), (41, 7, 0, false));
    let s = d.standard.unwrap();
    assert_eq!((s.mtime, s.ctime), (EPOCH + 10_000_000, EPOCH + 20_000_000));
    assert_eq!((s.attributes, s.security), (0x20, Some(261)));
    let names: Vec<_> = d.names.iter().map(|n| (n.parent, n.name.clone())).collect();
    assert_eq!(
        names,
        vec![
            (reference(40, 2), wtf8(&padding)),
            (reference(40, 2), wtf8(long))
        ],
        "the short name left out"
    );
    assert_eq!(d.data, Some((5, 8)));
}

/// A sector written apart from the others is torn, never read; a record
/// NTFS marked bad likewise; zeroes are no record, and one deleted is in
/// no use.
#[test]
fn a_torn_record_is_never_read() {
    let attributes = [standard(EPOCH, EPOCH, 0x20, 0)];
    let mut torn = record(1, IN_USE, 0, &attributes);
    torn[STRIDE * 2 - 1] ^= 0xFF;
    assert_eq!(decode(30, &mut torn), Err(Bad::Torn));
    let mut bad = record(1, IN_USE, 0, &attributes);
    bad[0..4].copy_from_slice(b"BAAD");
    assert_eq!(decode(30, &mut bad), Err(Bad::Torn));
    assert_eq!(decode(30, &mut vec![0u8; RECORD]), Err(Bad::Empty));
    let mut deleted = record(1, 0, 0, &attributes);
    assert_eq!(decode(30, &mut deleted), Err(Bad::Unused));
    // An attribute that says it is longer than the record.
    let mut long = record(1, IN_USE, 0, &attributes);
    let first = usize::from(u16_at(&long, 0x14).unwrap());
    long[first + 4..first + 8].copy_from_slice(&5000u32.to_le_bytes());
    assert_eq!(decode(30, &mut long), Err(Bad::Malformed));
}

/// Runs read back: lengths and offsets of several sizes, an offset going
/// back (negative), a sparse run, and the end.
#[test]
fn runs_read_back() {
    let bytes = [
        0x21, 0x10, 0x00, 0x01, // 16 clusters at 256
        0x32, 0x00, 0x01, 0x00, 0x00, 0x01, // 256 clusters at 256 + 65536
        0x01, 0x08, // 8 sparse clusters
        0x11, 0x04, 0xF0, // 4 clusters, 16 back
        0x00,
    ];
    let read = runs(&bytes).unwrap();
    assert_eq!(
        read,
        vec![
            Run {
                lcn: Some(256),
                clusters: 16
            },
            Run {
                lcn: Some(256 + 65_536),
                clusters: 256
            },
            Run {
                lcn: None,
                clusters: 8
            },
            Run {
                lcn: Some(256 + 65_536 - 16),
                clusters: 4
            },
        ]
    );
    assert_eq!(runs(&[0x21, 0x10]), Err(Bad::Malformed), "cut short");
    assert_eq!(
        runs(&[0x11, 0x01, 0x80, 0x00]),
        Err(Bad::Malformed),
        "before the volume"
    );
}

/// The table's runs read as a volume holds them, its clusters of 512
/// bytes and its records of 1 KiB: a run of three clusters ends within a
/// record, which the next run ends, and every record after keeps its
/// number. One torn is said with where it is, to be read again; one torn
/// across two runs, with nowhere. Chunks smaller than a run, or than a
/// record, read alike.
#[test]
fn a_record_split_between_two_runs_keeps_every_number() {
    const CLUSTER: u64 = 512;
    let made = |name: &str| {
        record(
            1,
            IN_USE,
            0,
            &[
                standard(EPOCH, EPOCH, 0x20, 0),
                file_name(reference(ROOT, 5), name, 1),
            ],
        )
    };
    // Records 0 to 4, two clusters each, over runs of 3, 1, 2 and 4
    // clusters: record 1 split between the first two.
    let runs = [(10, 3), (20, 1), (30, 2), (40, 4)].map(|(lcn, clusters)| Run {
        lcn: Some(lcn),
        clusters,
    });
    let mut records: Vec<Vec<u8>> = (0..5).map(|n| made(&format!("record {n}"))).collect();
    // Record 3 torn, whole in the last run.
    records[3][STRIDE - 1] ^= 0xFF;
    let flat: Vec<u8> = records.concat();
    let mut disk = vec![0u8; 50 * CLUSTER as usize];
    let mut at = 0;
    for run in &runs {
        let start = usize::try_from(run.lcn.unwrap() * CLUSTER).unwrap();
        let len = usize::try_from(run.clusters * CLUSTER).unwrap();
        disk[start..start + len].copy_from_slice(&flat[at..at + len]);
        at += len;
    }
    let read = |disk: &[u8], chunk: usize| {
        let mut buffer = vec![0u8; chunk];
        super::read_runs(&runs, CLUSTER, RECORD, 5, &mut buffer, &mut |at, bytes| {
            let at = usize::try_from(at).unwrap();
            bytes.copy_from_slice(&disk[at..at + bytes.len()]);
            Ok(())
        })
        .unwrap()
    };
    for chunk in [1 << 20, 1024, 512] {
        let whole = read(&disk, chunk);
        let names: Vec<(u64, Vec<u8>)> = whole
            .decoded
            .iter()
            .map(|d| (d.number, d.names[0].name.clone()))
            .collect();
        assert_eq!(
            names,
            [0, 1, 2, 4]
                .map(|n| (n, wtf8(&format!("record {n}"))))
                .to_vec(),
            "chunks of {chunk}"
        );
        assert_eq!(
            whole.unread,
            vec![(3, Some(40 * CLUSTER))],
            "chunks of {chunk}"
        );
        // Record 1's second sector torn, in the second run: unread, with
        // nowhere to read it again whole.
        let mut torn = disk.clone();
        torn[20 * CLUSTER as usize + STRIDE - 1] ^= 0xFF;
        assert_eq!(
            read(&torn, chunk).unread,
            vec![(1, None), (3, Some(40 * CLUSTER))],
            "chunks of {chunk}"
        );
    }
}

/// The records of [`the_table_is_the_tree_its_names_make`], by number.
fn tree_records() -> Vec<(u64, Vec<u8>)> {
    let root = reference(ROOT, 5);
    vec![
        (
            ROOT,
            record(
                5,
                FOLDER,
                0,
                &[standard(EPOCH, EPOCH, 0x06, 0), file_name(root, ".", 3)],
            ),
        ),
        (
            0,
            record(
                1,
                IN_USE,
                0,
                &[standard(EPOCH, EPOCH, 0x06, 0), file_name(root, "$MFT", 3)],
            ),
        ),
        (
            30,
            record(
                1,
                FOLDER,
                0,
                &[
                    standard(EPOCH, EPOCH, 0x20, 0),
                    file_name(root, "Projets", 1),
                ],
            ),
        ),
        (
            31,
            record(
                4,
                IN_USE,
                0,
                &[
                    standard(EPOCH + 1, EPOCH + 2, 0x20, 0),
                    file_name(reference(30, 1), "notes 1.txt", 1),
                    file_name(root, "second name.txt", 0),
                ],
            ),
        ),
        // 31's data, in a record of its own.
        (
            32,
            record(
                1,
                IN_USE,
                reference(31, 4),
                &[nonresident(0, 300_000, 303_104, &[0x21, 0x4A, 0x00, 0x02])],
            ),
        ),
        // Its parent was record 33 at sequence 1; 33 is another file now.
        (
            33,
            record(
                2,
                FOLDER,
                0,
                &[
                    standard(EPOCH, EPOCH, 0x20, 0),
                    file_name(root, "reused", 1),
                ],
            ),
        ),
        (
            34,
            record(
                1,
                IN_USE,
                0,
                &[
                    standard(EPOCH, EPOCH, 0x20, 0),
                    file_name(reference(33, 1), "stale", 1),
                ],
            ),
        ),
        // Its parent is a file.
        (
            35,
            record(
                1,
                IN_USE,
                0,
                &[
                    standard(EPOCH, EPOCH, 0x20, 0),
                    file_name(reference(31, 4), "inside a file", 1),
                ],
            ),
        ),
    ]
}

/// The tree the records' names make: a file of two names in both its
/// folders; an extension record's size given to its base; a record whose
/// parent's record was reused for another file, and the volume's metadata,
/// in no folder; a folder's link to a parent that is a file, in none.
#[test]
fn the_table_is_the_tree_its_names_make() {
    let root = reference(ROOT, 5);
    let mut made = tree_records();
    let decoded = made
        .iter_mut()
        .map(|(n, r)| decode(*n, r).unwrap())
        .collect::<Vec<_>>();
    let table = Table::of(decoded, vec![99]);
    assert_eq!(table.unread, vec![99]);
    let below: BTreeMap<PathBuf, (u64, u64, Time)> = table
        .below(root)
        .into_iter()
        .map(|(p, r)| (p, (number(r.reference), r.size, r.mtime)))
        .collect();
    let notes = (31, 300_000, super::super::time(EPOCH + 1));
    let expected = BTreeMap::from([
        (PathBuf::from("Projets"), (30, 0, Time::default())),
        (PathBuf::from(r"Projets\notes 1.txt"), notes),
        (PathBuf::from("second name.txt"), notes),
        (PathBuf::from("reused"), (33, 0, Time::default())),
    ]);
    assert_eq!(below, expected);
    assert!(
        table.get(reference(33, 1)).is_none(),
        "the reused record is another file"
    );
    assert_eq!(table.get(reference(31, 4)).unwrap().alloc, 303_104);
    // A folder's attributes say it is one, as a listing's do, though its
    // record keeps that in its head alone (its standard information as NTFS
    // writes it, here without).
    let folder = table.get(reference(30, 1)).unwrap();
    assert_ne!(folder.attributes & FILE_ATTRIBUTE_DIRECTORY, 0);
    assert_eq!(
        table.get(reference(31, 4)).unwrap().attributes & FILE_ATTRIBUTE_DIRECTORY,
        0
    );
}

/// The table's first record says where the table is: its data's runs.
#[test]
fn the_first_record_says_where_the_table_is() {
    let mut r = record(
        1,
        IN_USE,
        0,
        &[
            standard(EPOCH, EPOCH, 0x06, 0),
            file_name(reference(ROOT, 5), "$MFT", 3),
            nonresident(
                0,
                262_144,
                262_144,
                &[0x21, 0x40, 0x00, 0x0C, 0x11, 0x20, 0x40],
            ),
        ],
    );
    let d = decode(0, &mut r).unwrap();
    assert!(!d.listed);
    assert_eq!(
        d.runs,
        Some(vec![
            Run {
                lcn: Some(0xC00),
                clusters: 0x40
            },
            Run {
                lcn: Some(0xC40),
                clusters: 0x20
            },
        ])
    );
}

/// What a walk of a folder finds on a real NTFS volume, its file table
/// holds below that folder, read from the disk: each entry's path, kind,
/// identity and size. A name with spaces and accents, a file past what a
/// record holds, a file of two names, a link to a folder, an empty folder,
/// a path past 260 characters. On a test machine's volume alone
/// (`skwird_testkit::ntfs`), as only an administrator may read a volume.
#[test]
fn a_volumes_table_holds_what_a_walk_finds() {
    let Some(volume) = skwird_testkit::ntfs() else {
        return;
    };
    let s = Scratch::under(&volume, "mft");
    s.write(r"tree\a b\été 2026\notes 1.txt", b"hello");
    s.write(r"tree\a b\big.bin", &vec![7u8; 300_000]);
    s.dir(r"tree\empty folder");
    s.hard_link(
        r"tree\a b\second name.txt",
        r"tree\a b\été 2026\notes 1.txt",
    );
    s.link(r"tree\a b\lien", "été 2026");
    s.deep(r"tree\deep", 30, "un dossier");
    let walked = skwird_testkit::reference_walk(&s.join("tree"));
    let top = skwird_testkit::reference_walk(s.path()).entries[Path::new("tree")].ino;
    let table = Volume::table_of(&volume).unwrap();
    assert!(table.len() > 50, "a volume holds more: {}", table.len());
    let found: BTreeMap<PathBuf, (Kind, u64, u64)> = table
        .below(top)
        .into_iter()
        .map(|(p, r)| {
            let kind = if r.is_reparse() {
                Kind::Link
            } else if r.folder {
                Kind::Folder
            } else {
                Kind::File
            };
            (p, (kind, r.reference, if r.folder { 0 } else { r.size }))
        })
        .collect();
    let expected: BTreeMap<PathBuf, (Kind, u64, u64)> = walked
        .entries
        .iter()
        .map(|(p, seen)| (p.clone(), (seen.kind, seen.ino, seen.size)))
        .collect();
    assert_eq!(found, expected);
}

/// What a crawl found, folder by folder, in terms both ways of reading a
/// folder must agree on: each folder's identity and times, or why it is
/// not covered; each entry's name, kind and identity, and each file's size
/// and last write.
type Summary = BTreeMap<PathBuf, (Option<(FileId, Time, Time)>, Result<Vec<Seen>, Why>)>;
type Seen = (Vec<u8>, crate::Kind, FileId, Option<(u64, Time)>);

fn summary(crawl: &crate::Crawl) -> Summary {
    crawl
        .folders
        .iter()
        .map(|f| {
            let stat = f.stat.map(|s| (s.id, s.ctime, s.mtime));
            let outcome = match &f.outcome {
                Outcome::NotCovered(why) => Err(*why),
                Outcome::Listed(listing) => Ok(listing
                    .iter()
                    .map(|(name, e)| {
                        let file = e
                            .attrs
                            .filter(|_| e.kind == crate::Kind::File)
                            .map(|a| (a.size, a.mtime));
                        (name.to_vec(), e.kind, e.id, file)
                    })
                    .collect()),
            };
            (f.rel.clone(), (stat, outcome))
        })
        .collect()
}

/// A crawl of a folder read from its volume's file table is the crawl
/// read from its folders' own listings, on a real NTFS volume: the same
/// folders, the same identities and times, the same left out and why (a
/// `.private` folder, an exclusion, a junction and links not entered), the
/// same entries. On a test machine's volume alone (`skwird_testkit::ntfs`).
#[test]
fn a_crawl_of_the_table_is_a_crawl_of_the_folders() {
    let Some(volume) = skwird_testkit::ntfs() else {
        return;
    };
    let s = Scratch::under(&volume, "mft-crawl");
    s.write(r"tree\a b\été 2026\notes 1.txt", b"hello");
    s.write(r"tree\a b\big.bin", &vec![7u8; 300_000]);
    s.hard_link(
        r"tree\a b\second name.txt",
        r"tree\a b\été 2026\notes 1.txt",
    );
    s.link(r"tree\a b\lien", "été 2026");
    s.link(r"tree\a b\lien vers big", "big.bin");
    s.file(r"tree\journal intime\.private");
    s.file(r"tree\journal intime\entrée.txt");
    s.write(r"tree\clés\key.pem", b"k");
    s.deep(r"tree\deep", 12, "un dossier");
    let made = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "New-Item -ItemType Junction -Path '{}' -Target '{}' | Out-Null",
                s.join(r"tree\jonction").display(),
                s.join(r"tree\a b").display()
            ),
        ])
        .status()
        .unwrap();
    assert!(made.success(), "the junction");
    let (exclusions, refused) = crate::Exclusions::resolve([s.join(r"tree\clés")]);
    assert!(refused.is_empty(), "{refused:?}");
    let root = crate::Root::open(&s.join("tree"), exclusions).unwrap();
    let opts = Options {
        threads: 2,
        ..Options::default()
    };
    let cancel = crate::Cancel::default();
    let walked = crate::crawl_all(&root, &opts, &cancel).unwrap();
    let table = Volume::table_of(&volume).unwrap();
    let reader = TableReader { table: &table };
    let read = crate::crawl_below_by(
        &root,
        Path::new(""),
        None,
        &opts,
        &cancel,
        &|r, rel, id, o| reader.read(r, rel, id, o),
    )
    .unwrap();
    // A file's count of names, the table's alone: 2 for the file of two
    // names, by either; a listing says none.
    let links = |crawl: &crate::Crawl, folder: &str, name: &str| {
        crawl
            .folders
            .iter()
            .find(|f| f.rel == Path::new(folder))
            .and_then(|f| match &f.outcome {
                Outcome::Listed(listing) => listing
                    .iter()
                    .find(|(n, _)| *n == name.as_bytes())
                    .and_then(|(_, e)| e.attrs),
                Outcome::NotCovered(_) => None,
            })
            .map(|a| a.links)
    };
    assert_eq!(links(&read, "a b", "second name.txt"), Some(2));
    assert_eq!(links(&read, r"a b\été 2026", "notes 1.txt"), Some(2));
    assert_eq!(links(&read, "a b", "big.bin"), Some(1));
    assert_eq!(links(&walked, "a b", "second name.txt"), Some(0));
    let (walked, read) = (summary(&walked), summary(&read));
    // The root first, with both volumes' names: what the rest hangs on.
    assert_eq!(
        read.get(Path::new("")),
        walked.get(Path::new("")),
        "the root, its volume {:#x} by a stat, {:#x} by the table; {} records, {} unread",
        root.stat.id.dev,
        table.serial,
        table.len(),
        table.unread.len()
    );
    let why = |p: &str| read.get(Path::new(p)).map(|f| f.1.clone().err());
    assert_eq!(why("journal intime"), Some(Some(Why::Private)));
    assert_eq!(why("clés"), Some(Some(Why::Excluded)));
    assert!(read.len() > 12, "{read:?}");
    assert_eq!(read, walked);
}

/// What each folder is judged by, for the system service (DESIGN 4): its
/// security id, the same read from its volume's table and from its own
/// record; the descriptor each id names, fetched once, held by the root.
/// Two folders of the same access share an id; one given its own access
/// has another. On a test machine's volume alone.
#[test]
fn each_folder_says_what_it_is_judged_by() {
    let Some(volume) = skwird_testkit::ntfs() else {
        return;
    };
    let s = Scratch::under(&volume, "mft-security");
    s.dir(r"tree\shared one");
    s.dir(r"tree\shared two");
    let own = s.dir(r"tree\its own");
    // Its access its own: inherited no more, the administrators' alone.
    let granted = std::process::Command::new("icacls")
        .arg(&own)
        .args(["/inheritance:r", "/grant:r", "*S-1-5-32-544:(OI)(CI)F"])
        .output()
        .unwrap();
    assert!(
        granted.status.success(),
        "{}",
        String::from_utf8_lossy(&granted.stdout)
    );
    let root = crate::Root::open(&s.join("tree"), crate::Exclusions::none()).unwrap();
    let opts = Options {
        threads: 2,
        acls: true,
        ..Options::default()
    };
    let cancel = crate::Cancel::default();
    let owners = |crawl: &crate::Crawl| -> BTreeMap<PathBuf, u32> {
        crawl
            .folders
            .iter()
            .filter_map(|f| Some((f.rel.clone(), f.stat?.owner)))
            .collect()
    };
    let read = owners(&crate::crawl_all(&root, &opts, &cancel).unwrap());
    let table = Volume::table_of(&volume).unwrap();
    let reader = TableReader { table: &table };
    let from_table = owners(
        &crate::crawl_below_by(
            &root,
            Path::new(""),
            None,
            &opts,
            &cancel,
            &|r, rel, id, o| reader.read(r, rel, id, o),
        )
        .unwrap(),
    );
    assert_eq!(from_table, read, "the same ids both ways");
    assert_eq!(read.len(), 4, "{read:?}");
    let id = |p: &str| read[Path::new(p)];
    assert!(read.values().all(|&id| id != 0), "{read:?}");
    assert_eq!(id("shared one"), id("shared two"));
    assert_ne!(id("its own"), id("shared one"));
    for (path, id) in &read {
        assert!(
            root.descriptor(*id).is_some(),
            "{}: no descriptor for {id}",
            path.display()
        );
    }
    assert_ne!(
        root.descriptor(id("its own")),
        root.descriptor(id("shared one"))
    );
}

/// The check by the table says stale what the check of the disk says,
/// after a tree changed behind it: a file made in one folder, a folder
/// renamed, one removed, one made in another, one left as it was. Each
/// folder is found by its name from the root's record down, and judged by
/// its identity, its kind and its change time. On a test machine's NTFS
/// volume alone.
#[test]
fn a_check_by_the_table_is_the_disks() {
    let Some(volume) = skwird_testkit::ntfs() else {
        return;
    };
    let s = Scratch::under(&volume, "mft-check");
    for rel in [
        r"tree\a\one.txt",
        r"tree\b\two.txt",
        r"tree\c\d\three.txt",
        r"tree\e\four.txt",
        r"tree\quiet\five.txt",
    ] {
        s.write(rel, b"x");
    }
    let root = crate::Root::open(&s.join("tree"), crate::Exclusions::none()).unwrap();
    let opts = Options {
        threads: 2,
        ..Options::default()
    };
    let cancel = crate::Cancel::default();
    let crawl = crate::crawl_all(&root, &opts, &cancel).unwrap();
    let mut stored = crate::Stored::default();
    for f in &crawl.folders {
        if let Some(stat) = f.stat {
            stored.push(
                &f.rel,
                crate::StoredFolder {
                    id: stat.id,
                    ctime: stat.ctime,
                    again: false,
                },
            );
        }
    }
    // The changes a tick after the crawl (the engine's, 2 s), as a check
    // meets them: a change within it may leave a folder's time as it was,
    // NTFS' clock moving by its ticks (15.6 ms), and the engine reads such
    // a folder again whatever its time says (`again`). Made at once, the
    // root's rename once passed for no change.
    std::thread::sleep(Options::default().tick);
    s.write(r"tree\a\new.txt", b"x");
    s.rename(r"tree\b", r"tree\b2");
    s.remove(r"tree\c\d");
    s.dir(r"tree\e\made");
    let said = |c: &crate::Checked| {
        c.stale
            .iter()
            .map(|&(i, why)| (stored.rel(i).into_owned(), why))
            .collect::<BTreeMap<_, _>>()
    };
    let disk = said(&crate::check(&root, &stored, &opts, &cancel).unwrap());
    let table = Volume::table_of(&volume).unwrap();
    let by_table = said(&table.check(&root, &stored, &cancel).unwrap());
    assert_eq!(by_table, disk);
    for rel in ["", "a", "b", "c", r"c\d", "e"] {
        assert!(disk.contains_key(Path::new(rel)), "{rel}: {disk:?}");
    }
    assert!(!disk.contains_key(Path::new("quiet")), "{disk:?}");
}
