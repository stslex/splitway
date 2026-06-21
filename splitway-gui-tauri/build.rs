fn main() {
    // Generates the ACL/permission schemas from `tauri.conf.json` + the
    // `capabilities/` directory and wires the build metadata. Runs on all
    // platforms (no cfg guards — Tauri handles platform specifics internally).
    tauri_build::build()
}
