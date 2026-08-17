//! Ground truth for the bar clock, read out of rekordbox's own analysis.
//!
//! rekordbox has already solved offline what the daemon is trying to solve live:
//! its beat grid records, for every beat of every analysed track, that beat's
//! position within the bar. That makes it a reference to score an estimator
//! against — no hooking, nothing running, and no hand annotation.
//!
//! Emits a TSV so `audio-bridge`'s validation never depends on rekordbox being
//! installed. Read-only against `master.db`.
//!
//! **PQTZ only.** `rekordcrate` 0.3 parses the PSSI song-structure section, and
//! even handles its RB6+ encryption, but `SongStructure` keeps every field
//! private with no accessor, so the phrase labels cannot be read without forking
//! the crate. That costs less than it sounds: the beat grid numbers beats within
//! the bar, so counting from the first downbeat gives a 16-beat grid anchored at
//! the track's own start. For dance music that is phrase-aligned in practice —
//! an assumption worth stating rather than a guarantee, and the reason
//! `phrase_phase` below is derived rather than read.

use anyhow::{bail, Context, Result};
use binrw::BinRead;
use clap::{Parser, Subcommand};
use rekord_ripper::db::MasterDb;
use rekordcrate::anlz::{Content, ANLZ};
use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(about = "Beat-grid ground truth from the local rekordbox library")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
    /// Where to look for the audio when the path in the database points at
    /// another machine. Repeatable. Files are matched on filename, then on
    /// filename without the extension, so an mp3 in the database resolves to the
    /// flac of the same name on disk.
    #[arg(long = "audio-root", global = true)]
    audio_root: Vec<PathBuf>,
}

/// Filename to path, for relocating audio a travelled library has lost track of.
struct AudioIndex {
    by_name: HashMap<String, PathBuf>,
    by_stem: HashMap<String, PathBuf>,
}

impl AudioIndex {
    fn build(roots: &[PathBuf]) -> Self {
        let mut by_name = HashMap::new();
        let mut by_stem = HashMap::new();
        for root in roots {
            walk(root, &mut |p: &Path| {
                let audio = p
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| {
                        matches!(
                            e.to_lowercase().as_str(),
                            "flac" | "mp3" | "wav" | "m4a" | "aiff" | "aif" | "ogg"
                        )
                    })
                    .unwrap_or(false);
                if !audio {
                    return;
                }
                if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                    by_name.entry(n.to_lowercase()).or_insert_with(|| p.to_owned());
                }
                if let Some(s) = p.file_stem().and_then(|s| s.to_str()) {
                    by_stem.entry(s.to_lowercase()).or_insert_with(|| p.to_owned());
                }
            });
        }
        AudioIndex { by_name, by_stem }
    }

    fn len(&self) -> usize {
        self.by_name.len()
    }

    /// The recorded path if it is really there, else the same filename anywhere
    /// under the roots, else the same name with a different extension.
    fn resolve(&self, recorded: Option<&str>) -> Option<PathBuf> {
        let recorded = recorded?;
        let p = Path::new(recorded);
        if p.exists() {
            return Some(p.to_owned());
        }
        // Windows paths in the database still use forward slashes, and a
        // `soundcloud:tracks:…` reference has no filename at all.
        let name = recorded.rsplit(['/', '\\']).next()?;
        if let Some(hit) = self.by_name.get(&name.to_lowercase()) {
            return Some(hit.clone());
        }
        let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
        self.by_stem.get(&stem.to_lowercase()).cloned()
    }
}

fn walk(dir: &Path, f: &mut impl FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        match p.is_dir() {
            true => walk(&p, f),
            false => f(&p),
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// List analysed tracks, so you can find one to dump.
    List {
        /// Keep only titles or artists containing this, case-insensitive.
        query: Option<String>,
    },
    /// Write a beat-grid TSV for one track.
    Dump {
        /// Title or artist substring, case-insensitive. Must match one track.
        query: String,
        /// Where to write. Defaults to stdout.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
}

struct Track {
    title: String,
    artist: String,
    /// Hundredths of a BPM, as rekordbox stores it.
    bpm: Option<i64>,
    anlz: String,
    /// The audio itself. Recorded in the TSV so a truth file names what it is
    /// truth *about* — the scorer needs the two together and nothing else links
    /// them.
    folder: Option<String>,
}

/// Analysed tracks only — a track with no `AnalysisDataPath` has no beat grid,
/// so it cannot be ground truth for anything.
fn tracks(db: &MasterDb, query: Option<&str>) -> Result<Vec<Track>> {
    let mut stmt = db.conn.prepare(
        "SELECT c.Title, a.Name AS Artist, c.BPM, c.AnalysisDataPath, c.FolderPath
         FROM djmdContent c
         LEFT JOIN djmdArtist a ON a.ID = c.ArtistID
         WHERE c.AnalysisDataPath IS NOT NULL AND c.AnalysisDataPath <> ''
         ORDER BY a.Name, c.Title",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Track {
            title: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            artist: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            bpm: r.get(2)?,
            anlz: r.get(3)?,
            folder: r.get(4)?,
        })
    })?;

    let needle = query.map(str::to_lowercase);
    let mut out = Vec::new();
    for t in rows {
        let t = t?;
        let keep = match &needle {
            None => true,
            Some(n) => {
                t.title.to_lowercase().contains(n) || t.artist.to_lowercase().contains(n)
            }
        };
        if keep {
            out.push(t);
        }
    }
    Ok(out)
}

fn bpm_str(bpm: Option<i64>) -> String {
    bpm.map(|b| format!("{:.2}", b as f64 / 100.0))
        .unwrap_or_else(|| "-".into())
}

/// The beats of a track, in file order. `beat_number` is 1..4, its position in
/// the bar, which is the whole reason this tool exists.
fn beat_grid(db: &MasterDb, track: &Track) -> Result<Vec<(u32, u16)>> {
    let path = db.resolve_analysis_path(&track.anlz);
    let bytes = fs::read(&path)
        .with_context(|| format!("reading ANLZ at {}", path.display()))?;
    let anlz = ANLZ::read(&mut Cursor::new(&bytes))
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;

    for section in &anlz.sections {
        if let Content::BeatGrid(grid) = &section.content {
            return Ok(grid.beats.iter().map(|b| (b.time, b.beat_number)).collect());
        }
    }
    bail!(
        "no PQTZ beat grid in {} — the track is in the database but not analysed",
        path.display()
    )
}

fn dump(
    db: &MasterDb,
    index: &AudioIndex,
    query: &str,
    out: Option<PathBuf>,
) -> Result<()> {
    let found = tracks(db, Some(query))?;
    if found.is_empty() {
        bail!("nothing matches {query:?}");
    }

    // A library that has moved between machines holds a row per machine for the
    // same song — same title, different absolute path, its own analysis
    // directory. Resolve each to real audio first and judge ambiguity on that:
    // four rows that all land on one file are duplicates, not an ambiguous query.
    let resolved: Vec<(&Track, Option<PathBuf>)> = found
        .iter()
        .map(|t| (t, index.resolve(t.folder.as_deref())))
        .collect();
    let hits: Vec<&(&Track, Option<PathBuf>)> =
        resolved.iter().filter(|(_, p)| p.is_some()).collect();

    let candidates: Vec<&(&Track, Option<PathBuf>)> = match hits.is_empty() {
        true => resolved.iter().collect(),
        false => hits,
    };

    let mut files: Vec<String> = candidates
        .iter()
        .map(|(t, p)| match p {
            Some(p) => p.display().to_string(),
            None => t.folder.clone().unwrap_or_default(),
        })
        .collect();
    files.sort_unstable();
    files.dedup();
    if files.len() > 1 {
        eprintln!("{} distinct tracks match {query:?}:", files.len());
        for f in files.iter().take(12) {
            eprintln!("  {f}");
        }
        bail!("narrow the query so it matches one track");
    }
    let (track, audio) = candidates[0];
    let beats = beat_grid(db, track)?;
    if beats.is_empty() {
        bail!("the beat grid for {:?} is empty", track.title);
    }

    // Anchor the phrase grid on the first beat the grid calls beat 1. Counting
    // from the file's first entry instead would put the anchor wherever rekordbox
    // happened to start, which is not a downbeat.
    let first_downbeat = beats
        .iter()
        .position(|&(_, n)| n == 1)
        .context("no beat is numbered 1, so there is no downbeat to anchor to")?;

    let mut s = String::new();
    s.push_str(&format!("# artist\t{}\n", track.artist));
    s.push_str(&format!("# title\t{}\n", track.title));
    s.push_str(&format!("# bpm\t{}\n", bpm_str(track.bpm)));
    s.push_str(&format!("# beats\t{}\n", beats.len()));
    // The resolved path, not the recorded one — the scorer has to be able to open
    // it, and on a travelled library those are rarely the same string.
    s.push_str(&format!(
        "# file\t{}\n",
        audio
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".into())
    ));
    s.push_str("# phrase_phase is derived: beats from the first downbeat, mod 16\n");
    s.push_str("time_ms\tbeat_in_bar\tbeat_in_track\tphrase_phase\n");
    for (i, &(time, beat_in_bar)) in beats.iter().enumerate() {
        // Signed, so beats before the first downbeat get a negative index rather
        // than wrapping into a wrong phase.
        let n = i as i64 - first_downbeat as i64;
        let phase = n.rem_euclid(16);
        s.push_str(&format!("{time}\t{beat_in_bar}\t{n}\t{phase}\n"));
    }

    match out {
        Some(p) => {
            fs::write(&p, s).with_context(|| format!("writing {}", p.display()))?;
            eprintln!(
                "{} beats for {} — {} → {}",
                beats.len(),
                track.artist,
                track.title,
                p.display()
            );
        }
        None => print!("{s}"),
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Read-only, so a running rekordbox is not the hazard it is for a write.
    // Worth saying out loud because the WAL may hold writes we cannot see.
    if rekord_ripper::db::rekordbox_running() {
        eprintln!("note: rekordbox is running; recent edits may not be in master.db yet");
    }
    let db = MasterDb::open()?;
    let index = AudioIndex::build(&args.audio_root);
    if !args.audio_root.is_empty() {
        eprintln!("indexed {} audio files under {} roots", index.len(), args.audio_root.len());
    }

    match args.cmd {
        Cmd::List { query } => {
            let found = tracks(&db, query.as_deref())?;
            // Whether the audio is reachable decides whether a track can be
            // scored at all. A library that has travelled between machines is
            // mostly grids without files: Windows paths, another Mac's paths, and
            // `soundcloud:tracks:…` for anything that was only ever streamed.
            let mut local = 0;
            for t in &found {
                let here = index.resolve(t.folder.as_deref()).is_some();
                local += u32::from(here);
                println!(
                    "{} {:>8}  {} — {}",
                    if here { "audio" } else { "  -  " },
                    bpm_str(t.bpm),
                    t.artist,
                    t.title
                );
            }
            println!(
                "\n{} analysed tracks, {local} with audio reachable on this machine",
                found.len()
            );
        }
        Cmd::Dump { query, out } => dump(&db, &index, &query, out)?,
    }
    Ok(())
}
