fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("reagent {}", env!("CARGO_PKG_VERSION"));
    }
}
