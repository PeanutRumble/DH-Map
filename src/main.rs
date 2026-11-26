mod heatmap;
mod plotter;

use clap::Parser;
use std::io::{self, Write};
use std::thread;

/// Ask for the sqlite path until it opens successfully.
fn prompt_db() -> (rusqlite::Connection, String) {
    let stdin = io::stdin();
    loop {
        println!("$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$");
        println!("$       Peanut's Distant Horizons Mapper       $");
        println!("$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$$");
        print!("Input path to or drop in DistantHorizons.sqlite: ");
        io::stdout().flush().ok();

        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                eprintln!("\nno input, exiting");
                std::process::exit(1);
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("failed to read input: {e}");
                std::process::exit(1);
            }
        }

        // tolerate quotes (drag-and-drop into a terminal) and a leading ~/
        let mut path = line.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
        if let Some(rest) = path.strip_prefix("~/") {
            if let Ok(home) = std::env::var("HOME") {
                path = format!("{home}/{rest}");
            }
        }
        if path.is_empty() {
            continue;
        }
        if !std::path::Path::new(&path).is_file() {
            eprintln!("not a file: {path}");
            continue;
        }
        match heatmap::open_db(&path) {
            Ok(conn) => return (conn, path),
            Err(e) => eprintln!("could not open {path}: {e}"),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = plotter::Args::parse();

    // 1. ask for the database path before anything else happens
    let (conn, db_path) = prompt_db();

    // 2. make sure heat.csv exists (with header) before the plotter starts tailing it
    let csv = heatmap::open_csv()?;

    // 3. tracker runs in the background and keeps printing to the terminal
    thread::spawn(move || {
        if let Err(e) = heatmap::run(conn, &db_path, csv) {
            eprintln!("tracker stopped: {e}");
        }
    });

    // 4. window runs on the main thread (required by winit on some platforms);
    //    closing it ends the whole program
    plotter::run(args)
}
