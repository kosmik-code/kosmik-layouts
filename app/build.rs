fn main() {
    // The control page is built into the program.
    println!("cargo:rerun-if-changed=../public");
    tauri_build::build()
}
