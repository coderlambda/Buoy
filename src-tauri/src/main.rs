// Prevent a console window on Windows in release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    durable_terminal_lib::run();
}
