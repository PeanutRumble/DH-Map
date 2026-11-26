use rusqlite::{Connection, OpenFlags, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ---- tunables ----
const SECTION: i64 = 64;           // blocks per section edge at DetailLevel 0
const WINDOW_MS: i64 = 20_000;     // how long a hit stays relevant
const TAU_MS: f64 = 8_000.0;       // decay time constant
const POST_GEN_MS: i64 = 15_000;   // ignore MODIFIED this soon after NEW_GEN
const MIN_HITS: u32 = 3;           // hits a section needs to count as near-field
const LINK: i64 = 3;               // sections within this distance join one blob
const MATCH_DIST: f64 = 400.0;     // max blocks a blob can move between updates and keep its ID
const TRACK_TTL_MS: i64 = 60_000;  // forget an ID after this long unseen
const PRINT_EVERY_MS: i64 = 1_000;
pub const CSV_PATH: &str = "heat.csv";

fn now_millis() -> i64 {
    SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap()
    .as_millis() as i64
}

struct Blob {
    x: f64,
    z: f64,
    weight: f64,
    sections: usize,
}

struct Track {
    id: u32,
    x: f64,
    z: f64,
    last_seen: i64,
}

/// Cluster recent near-field hits into blobs (one per roughly one player).
fn blobs(recent: &VecDeque<(i64, i64, i64)>, now: i64) -> Vec<Blob> {
    // decayed weight and hit count per section
    let mut per: HashMap<(i64, i64), (f64, u32)> = HashMap::new();
    for &(t, x, z) in recent {
        let e = per.entry((x, z)).or_insert((0.0, 0));
        e.0 += (-((now - t) as f64) / TAU_MS).exp();
        e.1 += 1;
    }
    per.retain(|_, v| v.1 >= MIN_HITS);

    // connected components (Chebyshev distance <= LINK sections)
    let keys: Vec<(i64, i64)> = per.keys().cloned().collect();
    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    let mut out = Vec::new();

    for &start in &keys {
        if seen.contains(&start) {
            continue;
        }
        let mut stack = vec![start];
        seen.insert(start);
        let (mut sw, mut sx, mut sz, mut n) = (0.0, 0.0, 0.0, 0usize);

        while let Some(k) = stack.pop() {
            let (w, _) = per[&k];
            sw += w;
            sx += w * (k.0 * SECTION + SECTION / 2) as f64;
            sz += w * (k.1 * SECTION + SECTION / 2) as f64;
            n += 1;
            for &o in &keys {
                if !seen.contains(&o)
                    && (o.0 - k.0).abs() <= LINK
                    && (o.1 - k.1).abs() <= LINK
                    {
                        seen.insert(o);
                        stack.push(o);
                    }
            }
        }
        out.push(Blob { x: sx / sw, z: sz / sw, weight: sw, sections: n });
    }

    out.sort_by(|a, b| b.weight.partial_cmp(&a.weight).unwrap());
    out
}

/// Give each blob a stable ID by greedy nearest-neighbor matching to existing tracks.
fn assign_ids(
    tracks: &mut Vec<Track>,
    next_id: &mut u32,
    blobs: &[Blob],
    now: i64,
) -> Vec<u32> {
    // all candidate (distance, blob, track) pairs within MATCH_DIST
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (bi, b) in blobs.iter().enumerate() {
        for (ti, t) in tracks.iter().enumerate() {
            let d = ((b.x - t.x).powi(2) + (b.z - t.z).powi(2)).sqrt();
            if d <= MATCH_DIST {
                pairs.push((d, bi, ti));
            }
        }
    }
    pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    let mut ids = vec![0u32; blobs.len()];
    let mut blob_done = vec![false; blobs.len()];
    let mut track_done = vec![false; tracks.len()];

    // closest pairs claim each other first
    for (_, bi, ti) in pairs {
        if blob_done[bi] || track_done[ti] {
            continue;
        }
        blob_done[bi] = true;
        track_done[ti] = true;
        tracks[ti].x = blobs[bi].x;
        tracks[ti].z = blobs[bi].z;
        tracks[ti].last_seen = now;
        ids[bi] = tracks[ti].id;
    }

    // unmatched blobs get new IDs
    for (bi, b) in blobs.iter().enumerate() {
        if !blob_done[bi] {
            let id = *next_id;
            *next_id += 1;
            tracks.push(Track { id, x: b.x, z: b.z, last_seen: now });
            ids[bi] = id;
        }
    }

    // expire stale tracks
    tracks.retain(|t| now - t.last_seen <= TRACK_TTL_MS);
    ids
}

/// Open the SQLite file read-only.
pub fn open_db(db_path: &str) -> Result<Connection> {
    Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

/// Open (or create) the CSV the plotter tails. Header written only when the file is new/empty.
pub fn open_csv() -> std::io::Result<File> {
    let mut csv = OpenOptions::new().create(true).append(true).open(CSV_PATH)?;
    if csv.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
        writeln!(csv, "time_ms,id,x,z,weight,sections")?;
    }
    Ok(csv)
}

/// Tail the database forever, printing blobs to the terminal and appending them to the CSV.
pub fn run(conn: Connection, db_path: &str, mut csv: File) -> Result<()> {
    // >= so rows sharing a millisecond aren't skipped; the checksum check makes re-reads harmless.
    let mut stmt = conn.prepare(
        "SELECT PosX, PosZ, LastModifiedUnixDateTime, CreatedUnixDateTime, DataChecksum
        FROM FullData
        WHERE DetailLevel = 0 AND LastModifiedUnixDateTime >= ?1
        ORDER BY LastModifiedUnixDateTime ASC",
    )?;

    // ---- state ----
    let mut last_seen = now_millis();
    let mut last_sum: HashMap<(i64, i64), i64> = HashMap::new(); // last checksum per section
    let mut gen_seen: HashMap<(i64, i64), i64> = HashMap::new(); // when each section was generated
    let mut recent: VecDeque<(i64, i64, i64)> = VecDeque::new(); // (time, posx, posz)
    let mut tracks: Vec<Track> = Vec::new();
    let mut next_id: u32 = 1;
    let mut last_print = 0i64;

    println!("Tailing {} -> logging to {} ...", db_path, CSV_PATH);

    loop {
        {
            let mut rows = stmt.query([last_seen])?;
            while let Some(row) = rows.next()? {
                let pos_x: i64 = row.get(0)?;
                let pos_z: i64 = row.get(1)?;
                let modified: i64 = row.get(2)?;
                let created: i64 = row.get(3)?;
                let sum: i64 = row.get(4)?;
                let key = (pos_x, pos_z);

                // did the content actually change since we last saw this section?
                let changed = last_sum.insert(key, sum) != Some(sum);

                if modified > created {
                    let post_gen = gen_seen
                    .get(&key)
                    .map_or(false, |&t| modified - t < POST_GEN_MS);
                    if changed && !post_gen {
                        recent.push_back((modified, pos_x, pos_z));
                    }
                } else {
                    gen_seen.insert(key, modified);
                }

                if modified > last_seen {
                    last_seen = modified;
                }
            }
        }

        let now = now_millis();

        // drop hits older than the window
        while let Some(&(t, _, _)) = recent.front() {
            if now - t > WINDOW_MS {
                recent.pop_front();
            } else {
                break;
            }
        }

        if now - last_print >= PRINT_EVERY_MS {
            last_print = now;

            let found = blobs(&recent, now);
            let ids = assign_ids(&mut tracks, &mut next_id, &found, now);

            if found.is_empty() {
                println!("[{}] no blobs ({} hits)", now, recent.len());
            } else {
                for (b, id) in found.iter().zip(ids.iter()) {
                    println!(
                        "[{}] blob #{}  X: {:.0}, Z: {:.0}  ({} sections, w={:.1})",
                             now, id, b.x, b.z, b.sections, b.weight
                    );
                    writeln!(
                        csv,
                        "{},{},{:.0},{:.0},{:.2},{}",
                        now, id, b.x, b.z, b.weight, b.sections
                    )
                    .unwrap();
                }
            }
        }

        thread::sleep(Duration::from_millis(500));
    }
}
