// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if picard_lib::cli::is_cli(&args) {
        std::process::exit(picard_lib::cli::main(&args));
    }
    picard_lib::run();
}
