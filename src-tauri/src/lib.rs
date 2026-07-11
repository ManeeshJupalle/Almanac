#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|_app| {
            // The shell is a client of the headless core (ARCHITECTURE.md:
            // "Headless-runnable core"). On startup it asks the core to open
            // the database and run pending migrations — no engine logic here.
            let db_path = almanac_core::init_default_db()?;
            println!("almanac: db ready at {}", db_path.display());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
